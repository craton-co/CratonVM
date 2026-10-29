// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP `ThreadReference.ForceEarlyReturn` (11/14; interpreter round i1 wave
//! 43, lane L1, stage 2 of
//! `docs/feature-designs/jvmti-delivery-threading.md`):
//! an IDE's "Force Return" — leave the thread's top method with a value the
//! debugger chooses.
//!
//! **Where it is served.** Only at the interpreter's suspend point before a
//! bytecode (`interpreter::deliver_breakpoint_if_set`: a breakpoint, a step,
//! a method entry, a plain suspension), which records each such park as a
//! *point* ([`enter_point`] / [`leave_point`], innermost last). The command
//! runs on the parked thread itself ([`super::run_on_parked_thread`]) and
//! checks that the thread's innermost park is such a point and that its top
//! frame is the one parked there, then records the value
//! ([`EarlyReturns::pending`]). When the debugger resumes the thread, the
//! park ends, the suspend point takes the value ([`take`]) and the dispatch
//! loop returns it from the frame as a return bytecode would
//! (`interpreter::force_return_here` / `finish_forced_return`): the block
//! monitors the frame holds are released, a `synchronized` method's monitor
//! with the frame, and `MethodExit` is reported with the value, as HotSpot
//! does. As on HotSpot the return happens at the resume, not at the command
//! (`tools/probes/interp/L1/L1W43JdiForceEarlyReturn.java`: each forced
//! `MethodExit` arrives after the resume).
//!
//! **Refused**, as HotSpot refuses what it cannot do: `OPAQUE_FRAME` for a
//! thread suspended in a native method (its top frame) or in compiled code,
//! and for one parked anywhere else (a field watch, an exception, a method
//! exit, a class prepare: the bytecode there has begun); `TYPE_MISMATCH` for
//! a value of the wrong kind for the method's return type (an `int`-kind tag
//! for a `boolean`, `byte`, `char`, `short` or `int` method, as JVMTI's
//! `ForceEarlyReturnInt` takes all five; `V` for a `void` method; an array for
//! an array type); `THREAD_NOT_SUSPENDED`, `INVALID_THREAD` and
//! `INVALID_OBJECT` as for other thread commands, except that a thread
//! running in a native method is `OPAQUE_FRAME` suspended or not; and
//! `INTERNAL` for a second request before the thread resumed, as HotSpot's
//! JVMTI answers it. Each measured on HotSpot 25.0.3
//! (`tools/probes/interp/L1/L1W43RawJdwpObjectErrorAnswers.java`). A reference is not checked
//! against the declared class, as the `SetValues` commands do not check it
//! (`debug::inspect`, module doc): JDI checks it before sending.

use std::collections::HashMap;

use super::commands::{
    read_tagged_value, thread_status_of, CommandResult, ERR_INTERNAL, ERR_INVALID_OBJECT,
    ERR_INVALID_TAG, ERR_INVALID_THREAD, ERR_OPAQUE_FRAME, ERR_THREAD_NOT_SUSPENDED,
    ERR_TYPE_MISMATCH, THREAD_STATUS_ZOMBIE,
};
use super::ids::ObjectExport;
use super::protocol::PayloadReader;
use super::{DebugState, DebuggerValue};
use crate::types::Value;

/// The session's forced returns (a [`DebugState`] field).
#[derive(Default)]
pub(crate) struct EarlyReturns {
    /// Thread id → the frame counts of the thread's parks at a bytecode
    /// about to run, innermost last (a park nests inside a debugger
    /// invocation run from an outer one), each with the index of the entry
    /// frame of the dispatch loop the park stopped (interpreter round i1
    /// wave 44, lane L1: `StackFrame.PopFrames` pops only frames whose caller
    /// that loop runs, `debug::pop_frames`).
    points: HashMap<u64, Vec<Point>>,
    /// Thread id → the frame count of the park it was recorded at and the
    /// value to return. An object value's id is pinned, its collection
    /// disabled, until it is taken (or the session ends).
    pending: HashMap<u64, (usize, DebuggerValue)>,
    /// Interpreter round i2 wave 61, lane L1: a C JVMTI agent's forced
    /// returns (`ForceEarlyReturn*`, [`record_jvmti`]): thread id → the frame
    /// count of the park it was recorded at and the value. An agent has no
    /// JDWP object id: an object is held by a JNI global reference this
    /// record owns until it is taken. Not a session's: a debugger's detach
    /// leaves it.
    pub(crate) jvmti_pending: HashMap<u64, (usize, JvmtiForced)>,
    /// Interpreter round i2 wave 61, lane L1: a C JVMTI agent's `PopFrame`s
    /// not taken yet (`debug::pop_frames::request_jvmti_pop`): thread id →
    /// the frame count of the park and whether the popped frame's invoke is
    /// an `invokestatic`. As on HotSpot the frame is popped when the thread
    /// resumes, not at the call.
    pub(crate) jvmti_pops: HashMap<u64, (usize, bool)>,
}

/// The value of a C JVMTI agent's forced return (interpreter round i2 wave
/// 61, lane L1; [`EarlyReturns::jvmti_pending`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum JvmtiForced {
    /// `ForceEarlyReturnVoid`.
    Void,
    /// A primitive, or a `null` object.
    Plain(Value),
    /// A non-null object, by a JNI global reference the record owns.
    Global(u64),
}

/// One park of a thread at a bytecode about to run ([`enter_point`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Point {
    /// The thread's frame count: at the park, or after the frames a
    /// `PopFrames` popped while parked there ([`note_frames_popped`]).
    pub(crate) depth: usize,
    /// The entry frame of the dispatch loop the park stopped (wave 44).
    pub(crate) loop_base: usize,
    /// A `PopFrames` popped frames while parked here (wave 44): the frame
    /// the park stopped is gone, so no forced return is recorded here (the
    /// suspend point, which takes one, answers the pop instead).
    pub(crate) popped: bool,
    /// Interpreter round i7 wave 137, lane L1
    /// (`CRATONVM_JVMTI_LAMBDA_ENTRY_POP`): the point is the `MethodEntry`
    /// callback of the entry frame of a door that calls its target again when
    /// that frame is popped (the lambda door, standing for the spun proxy's
    /// method, whose invoke HotSpot re-runs). A `PopFrame` of the frame at
    /// `loop_base` is admitted although its caller is below the loop
    /// (`pop_frames::request_jvmti_pop_in_callback`); the door takes it before
    /// the frame runs (`pop_frames::take_reinvokable_entry_pop`).
    pub(crate) reinvokable_entry: bool,
}

impl EarlyReturns {
    /// Forget every forced return (the debugger detached): the values'
    /// object ids go with the session's (`ObjectTable::release_all`).
    pub(crate) fn clear_pending(&mut self) {
        self.pending.clear();
    }
}

/// Thread `tid` parks at a bytecode about to run, with `depth` frames, in
/// the dispatch loop whose entry frame is frame `loop_base` (wave 44).
pub(crate) fn enter_point(shared: &crate::vm::SharedVm, tid: u64, depth: usize, loop_base: usize) {
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .points
        .entry(tid)
        .or_default()
        .push(Point {
            depth,
            loop_base,
            popped: false,
            reinvokable_entry: false,
        });
}

/// [`enter_point`] for the `MethodEntry` callback of a re-invoking door's
/// entry frame (interpreter round i7 wave 137, lane L1;
/// [`Point::reinvokable_entry`]): `loop_base` is that frame, `depth - 1`.
pub(crate) fn enter_reinvokable_point(shared: &crate::vm::SharedVm, tid: u64, depth: usize) {
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .points
        .entry(tid)
        .or_default()
        .push(Point {
            depth,
            loop_base: depth.saturating_sub(1),
            popped: false,
            reinvokable_entry: true,
        });
}

/// The innermost park of thread `tid` at a bytecode about to run, if its
/// innermost park is one (wave 44, `debug::pop_frames`).
pub(crate) fn innermost_point(ds: &DebugState, tid: u64) -> Option<Point> {
    ds.early_returns
        .points
        .get(&tid)
        .and_then(|points| points.last())
        .copied()
}

/// A `PopFrames` left thread `tid` with `depth` frames while it is parked at
/// its innermost point (wave 44): a second pop in the same suspension is
/// judged against the new count, and no forced return is recorded there.
pub(crate) fn note_frames_popped(ds: &mut DebugState, tid: u64, depth: usize) {
    if let Some(point) = ds
        .early_returns
        .points
        .get_mut(&tid)
        .and_then(|points| points.last_mut())
    {
        point.depth = depth;
        point.popped = true;
    }
}

/// Drop thread `tid`'s forced return not yet taken, releasing its object
/// (wave 44): a `PopFrames` removed the frame it was to return from. Also a
/// C agent's (wave 61) -- unless (wave 62) it was recorded for a frame the
/// pop leaves, one of the `remaining` frames: a `ForceEarlyReturn*` made
/// after a `PopFrame` of the same stop returns from the popped frame's
/// caller ([`record_jvmti`]).
pub(crate) fn discard_pending(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    tid: u64,
    remaining: usize,
) {
    if let Some((_, value)) = ds.early_returns.pending.remove(&tid) {
        if let Some(id) = object_id(&value) {
            release(shared, ds, id);
        }
    }
    if ds
        .early_returns
        .jvmti_pending
        .get(&tid)
        .is_some_and(|&(at, _)| at <= remaining)
    {
        return;
    }
    if let Some((_, JvmtiForced::Global(global))) = ds.early_returns.jvmti_pending.remove(&tid) {
        let _ = shared.natives.jni_global_refs.lock().remove(global);
    }
}

/// Is a forced return, a JDWP debugger's or a C agent's, recorded for thread
/// `tid` and not taken yet (wave 61)? A second request is `INTERNAL`.
fn has_pending(ds: &DebugState, tid: u64) -> bool {
    ds.early_returns.pending.contains_key(&tid) || ds.early_returns.jvmti_pending.contains_key(&tid)
}

/// Thread `tid` leaves the park [`enter_point`] recorded last.
pub(crate) fn leave_point(shared: &crate::vm::SharedVm, tid: u64) {
    let mut ds = shared.debug.debug_state.lock();
    if let Some(points) = ds.early_returns.points.get_mut(&tid) {
        points.pop();
        if points.is_empty() {
            ds.early_returns.points.remove(&tid);
        }
    }
}

/// Thread `tid` returns from a C agent's `Breakpoint` or `SingleStep`
/// callback for its top frame, with `depth` frames, and leaves the point
/// [`enter_point`] recorded for it (interpreter round i2 wave 62, lane L1;
/// `DebuggerGates::c_callback_frame_control`). Answers whether the callback
/// recorded a `PopFrame` or a forced return of that frame, which the
/// interpreter then takes ([`take`], `pop_frames::take_jvmti_pop`) before the
/// bytecode runs. One lock of the debug state.
pub(crate) fn leave_callback_point(shared: &crate::vm::SharedVm, tid: u64, depth: usize) -> bool {
    let mut ds = shared.debug.debug_state.lock();
    if let Some(points) = ds.early_returns.points.get_mut(&tid) {
        points.pop();
        if points.is_empty() {
            ds.early_returns.points.remove(&tid);
        }
    }
    ds.early_returns
        .jvmti_pending
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth)
        || ds
            .early_returns
            .jvmti_pops
            .get(&tid)
            .is_some_and(|&(at, _)| at == depth)
}

/// Is a C agent's forced return recorded for thread `tid`'s frame at
/// `depth` and not taken yet (wave 62: one made after a `PopFrame`, for the
/// popped frame's caller, [`record_jvmti`])?
pub(crate) fn jvmti_pending_at(shared: &crate::vm::SharedVm, tid: u64, depth: usize) -> bool {
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .jvmti_pending
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth)
}

/// The value a `ForceEarlyReturn` recorded for thread `tid`'s park with
/// `depth` frames, taken: `Some(None)` for a `void` method, `None` when none
/// was recorded for this park. An object is resolved now (its id's pin and
/// collection hold released); the caller pushes it onto the frame before
/// its next safepoint.
pub(crate) fn take(shared: &crate::vm::SharedVm, tid: u64, depth: usize) -> Option<Option<Value>> {
    let mut ds = shared.debug.debug_state.lock();
    if ds.early_returns.pending.is_empty() && ds.early_returns.jvmti_pending.is_empty() {
        return None;
    }
    // Wave 61 (lane L1): a C agent's.
    if let Some(&(at, forced)) = ds.early_returns.jvmti_pending.get(&tid) {
        if at == depth {
            ds.early_returns.jvmti_pending.remove(&tid);
            return Some(match forced {
                JvmtiForced::Void => None,
                JvmtiForced::Plain(v) => Some(v),
                JvmtiForced::Global(global) => {
                    let mut refs = shared.natives.jni_global_refs.lock();
                    let obj = refs.resolve(global);
                    let _ = refs.remove(global);
                    Some(Value::Object(obj))
                }
            });
        }
    }
    let (at, value) = ds.early_returns.pending.remove(&tid)?;
    if at != depth {
        // Recorded at another level of a nested park: that level takes it.
        ds.early_returns.pending.insert(tid, (at, value));
        return None;
    }
    let resolved = match value {
        DebuggerValue::Void => None,
        ref v => Some(
            super::inspect::debugger_value_to_vm(shared, &ds, v).unwrap_or(Value::Object(None)),
        ),
    };
    if let Some(id) = object_id(&value) {
        release(shared, &mut ds, id);
    }
    super::release_disposed_objects(shared, &mut ds);
    Some(resolved)
}

/// The non-null object id `value` holds, if it is a reference.
fn object_id(value: &DebuggerValue) -> Option<u64> {
    match *value {
        DebuggerValue::Object(id)
        | DebuggerValue::Array(id)
        | DebuggerValue::String(id)
        | DebuggerValue::Thread(id)
        | DebuggerValue::ThreadGroup(id)
        | DebuggerValue::ClassLoader(id)
        | DebuggerValue::ClassObject(id) => (id != 0).then_some(id),
        _ => None,
    }
}

/// Undo the hold [`record`] put on object id `id`.
fn release(shared: &crate::vm::SharedVm, ds: &mut DebugState, id: u64) {
    let _ = super::set_collection_enabled(shared, ds, id, true);
    ds.objects.unpin(id);
}

/// Can a value tagged `tag` be returned from a method whose return type
/// starts with `ret`? JVMTI's `ForceEarlyReturn*` families: `Int` for the
/// five `int`-kind types, `Long`, `Float`, `Double`, `Object` for a class or
/// array type, `Void` for `void`.
fn kind_fits(ret: u8, tag: u8) -> bool {
    const INT_KINDS: &[u8] = b"ZBCSI";
    const REFERENCES: &[u8] = b"L[stglc";
    match ret {
        b'V' => tag == b'V',
        b'J' | b'F' | b'D' => tag == ret,
        b'L' | b'[' => REFERENCES.contains(&tag),
        _ => INT_KINDS.contains(&ret) && INT_KINDS.contains(&tag),
    }
}

/// `ThreadReference.ForceEarlyReturn` (11/14): `thread`, then a tagged
/// value. See the module doc.
pub(crate) fn force_early_return(shared: &crate::vm::SharedVm, data: &[u8]) -> CommandResult {
    let mut r = PayloadReader::new(data);
    let Ok(tid) = r.read_u64_be() else {
        return CommandResult::error(ERR_INTERNAL);
    };
    let Ok(value) = read_tagged_value(&mut r) else {
        return CommandResult::error(ERR_INVALID_TAG);
    };
    let suspended = {
        let ds = shared.debug.debug_state.lock();
        match thread_status_of(&ds, tid) {
            None | Some(THREAD_STATUS_ZOMBIE) => return CommandResult::error(ERR_INVALID_THREAD),
            Some(_) => ds.is_thread_suspended(tid),
        }
    };
    if !suspended {
        // HotSpot's back end looks at the top frame first: a thread running
        // in a native method (blocked in `sleep`, `wait`, I/O) is
        // `OPAQUE_FRAME` whether or not it is suspended
        // (`L1W43RawJdwpObjectErrorAnswers`, measured on HotSpot 25.0.3).
        let blocked = shared
            .threads
            .thread_registry
            .is_blocked(crate::threading::ThreadId(tid));
        return CommandResult::error(if blocked {
            ERR_OPAQUE_FRAME
        } else {
            ERR_THREAD_NOT_SUSPENDED
        });
    }
    let recorded = super::run_on_parked_thread(shared, Some(tid), move |shared, thread| {
        record(shared, thread, tid, value)
    });
    match recorded {
        Ok(Ok(())) => CommandResult::ok(Vec::new()),
        Ok(Err(code)) => CommandResult::error(code),
        // Suspended, but not at an interpreter suspend point: blocked in a
        // native method, which heads its listing, or in compiled code.
        Err(super::ParkedError::NotParked) => {
            let suspended = shared.debug.debug_state.lock().is_thread_suspended(tid);
            CommandResult::error(if suspended {
                ERR_OPAQUE_FRAME
            } else {
                ERR_THREAD_NOT_SUSPENDED
            })
        }
        // Running a debugger invocation.
        Err(super::ParkedError::Busy) => CommandResult::error(ERR_THREAD_NOT_SUSPENDED),
        Err(super::ParkedError::Lost) => CommandResult::error(ERR_INTERNAL),
    }
}

/// Record a forced return of `value` for thread `tid`, on that thread while
/// it is parked (the JDWP answer's error code on refusal). Runs with the
/// suspend point suppressed and no collection possible (a registered
/// mutator between two polls), so an object id resolves and stays put.
fn record(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    tid: u64,
    value: DebuggerValue,
) -> Result<(), u16> {
    let depth = thread.frames.len();
    let top = thread.frames.last().ok_or(ERR_OPAQUE_FRAME)?;
    let ret = top.return_tag();
    let mut ds = shared.debug.debug_state.lock();
    let at_point = ds
        .early_returns
        .points
        .get(&tid)
        .and_then(|points| points.last())
        .is_some_and(|point| point.depth == depth && !point.popped);
    if !at_point {
        return Err(ERR_OPAQUE_FRAME);
    }
    if !kind_fits(ret, value.tag()) {
        return Err(ERR_TYPE_MISMATCH);
    }
    // A second request before the thread resumed: HotSpot 25.0.3 refuses it
    // as `INTERNAL` (measured, `L1W43RawJdwpObjectErrorAnswers`); a C
    // agent's counts too (wave 61).
    if has_pending(&ds, tid) {
        return Err(ERR_INTERNAL);
    }
    if let Some(id) = object_id(&value) {
        let obj = super::object_for_id(shared, &ds, id).ok_or(ERR_INVALID_OBJECT)?;
        if ret == b'[' && !super::inspect::is_array(shared, obj) {
            return Err(ERR_TYPE_MISMATCH);
        }
        if !super::set_collection_enabled(shared, &mut ds, id, false) {
            return Err(ERR_INVALID_OBJECT);
        }
        // Pinned as well: a debugger that disposes of the id must not free
        // its handle before the thread returns the object.
        ds.objects.note_export(id, ObjectExport::Pinned);
    }
    ds.early_returns.pending.insert(tid, (depth, value));
    Ok(())
}

/// Record a C JVMTI agent's forced return (`ForceEarlyReturn*`; interpreter
/// round i2 wave 61, lane L1) of `value`, of the family `kind` (`I` for
/// `ForceEarlyReturnInt`, `J`, `F`, `D`, `L` for `ForceEarlyReturnObject`,
/// `V` for `ForceEarlyReturnVoid`), for thread `tid`, on that thread while it
/// is parked. Taken at the resume as a JDWP debugger's is ([`take`]). The
/// refusals in HotSpot's order (`SetForceEarlyReturn::doit`,
/// `JvmtiEnvBase::check_top_frame`): a forced return already pending,
/// `INTERNAL`; a top frame that is not the one parked before a bytecode,
/// `OPAQUE_FRAME`; a value of another kind than the method's return type, or
/// an object its declared class does not name among the object's supertypes
/// (`VM_GetOrSetLocal::is_assignable` compares names), `TYPE_MISMATCH`. On
/// success the record owns `value`'s global reference; on refusal the caller
/// still does. Runs with no collection possible (a registered mutator
/// between two polls).
pub(crate) fn record_jvmti(
    shared: &crate::vm::SharedVm,
    thread: &crate::threading::jvm_thread::JvmThread,
    tid: u64,
    kind: u8,
    value: JvmtiForced,
) -> Result<(), u16> {
    let depth = thread.frames.len();
    let top = thread.frames.last().ok_or(ERR_OPAQUE_FRAME)?;
    let ret = top.return_tag();
    // Read before the debug-state lock: the class manager is not taken under
    // it (`debug::populate_class_metadata` nests them the other way round).
    let assignable = match value {
        JvmtiForced::Global(global) => {
            let obj = shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(global)
                .ok_or(ERR_INVALID_OBJECT)?;
            let descriptor = top.method_descriptor();
            let declared = descriptor.split_once(')').map_or("", |(_, r)| r);
            object_assignable(shared, obj, declared)
        }
        _ => true,
    };
    let mut ds = shared.debug.debug_state.lock();
    if has_pending(&ds, tid) {
        return Err(ERR_INTERNAL);
    }
    let at_point = ds
        .early_returns
        .points
        .get(&tid)
        .and_then(|points| points.last())
        .is_some_and(|point| point.depth == depth && !point.popped);
    if !at_point {
        return Err(ERR_OPAQUE_FRAME);
    }
    if !kind_fits(ret, kind) || !assignable {
        return Err(ERR_TYPE_MISMATCH);
    }
    // Interpreter round i2 wave 62, lane L1: after a `PopFrame` of this
    // stop the value is the popped frame's caller's to return, once the pop
    // is done, as HotSpot applies its pending early return to the frame on
    // top then (MEASURED, `L1W62JvmtiCallbackFrameControl`'s `bp pop then
    // int`: `popCaller` returns the value). The kind is checked against the
    // frame on top now, as HotSpot checks it. Until wave 62: `OPAQUE_FRAME`.
    let popped_first = ds
        .early_returns
        .jvmti_pops
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth);
    let at = if popped_first { depth - 1 } else { depth };
    ds.early_returns.jvmti_pending.insert(tid, (at, value));
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_EARLY_RETURN] recorded tid={tid} kind={} depth={depth}",
            kind as char
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Frame control of a thread blocked entering a monitor (interpreter round i6
// wave 134, lane L1;
// `docs/feature-designs/jvmti-delivery-threading.md`,
// `VmConfig::jvmti_blocked_entry_frame_control`)
// ---------------------------------------------------------------------------
//
// A thread suspended while blocked in a contended `monitorenter` (or a
// contended `synchronized` entry whose frame is pushed first) is at no suspend
// point, so `run_on_parked_thread` cannot reach it. On HotSpot its top frame is
// the interpreted frame at the monitor entry, and a `PopFrame` or a
// `ForceEarlyReturn*` is recorded and taken in the call epilogue of
// `InterpreterRuntime::monitorenter`, once the thread holds the monitor
// (`check_and_handle_popframe` / `check_and_handle_earlyret`); the pop or the
// return releases the monitor with the frame. Here the request is checked and
// recorded through the thread's inspection window
// (`jvmti::native_env::pop_through_window` / `forced_return_through_window`), kept in
// `DebugState::deferred_frame_pops` beside wave 61's `NotifyFramePop`s, and
// adopted by the thread at its wake ([`adopt_blocked_frame_control`]) as the
// record a callback point leaves: the dispatch loop's slow path then takes it
// before the next bytecode, as after a `MethodEntry` callback's request.

/// Is a C agent's `PopFrame` or forced return recorded for thread `tid`'s
/// frame at `depth`, not taken yet (wave 134)? One lock of the debug state.
pub(crate) fn jvmti_frame_control_at(shared: &crate::vm::SharedVm, tid: u64, depth: usize) -> bool {
    let ds = shared.debug.debug_state.lock();
    ds.early_returns
        .jvmti_pops
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth)
        || ds
            .early_returns
            .jvmti_pending
            .get(&tid)
            .is_some_and(|&(at, _)| at == depth)
}

/// The frame count a C agent's frame control recorded for thread `tid` is
/// for, not taken yet (interpreter round i7 wave 136, lane L1): a `PopFrame`'s
/// when one is recorded (a forced return made after it is for the popped
/// frame's caller and is taken once the pop is done), else a forced return's.
/// `None` when neither is recorded. The dispatch loop's checked reader of the
/// agent-request word asks it (`jvmti_events::take_method_entry_frame_control_checked`).
pub(crate) fn jvmti_frame_control_depth(shared: &crate::vm::SharedVm, tid: u64) -> Option<usize> {
    let ds = shared.debug.debug_state.lock();
    ds.early_returns
        .jvmti_pops
        .get(&tid)
        .map(|&(at, _)| at)
        .or_else(|| ds.early_returns.jvmti_pending.get(&tid).map(|&(at, _)| at))
}

/// Drop thread `tid`'s C agent frame control recorded for a frame count above
/// `depth` (interpreter round i7 wave 136, lane L1): the frames it named left
/// the stack without the loop top that takes it (the nested-entry native
/// stack trip pops an unrun frame; a frame popped meanwhile). A forced return
/// recorded after a dropped `PopFrame` goes with it (it was the popped
/// frame's caller's, conditional on the pop). The object of a forced return
/// is released. Answers whether anything was dropped.
pub(crate) fn discard_jvmti_frame_control_above(
    shared: &crate::vm::SharedVm,
    tid: u64,
    depth: usize,
) -> bool {
    let mut ds = shared.debug.debug_state.lock();
    let pop_dropped = match ds.early_returns.jvmti_pops.get(&tid) {
        Some(&(at, _)) if at > depth => {
            ds.early_returns.jvmti_pops.remove(&tid);
            Some(at)
        }
        _ => None,
    };
    let forced_dropped = ds
        .early_returns
        .jvmti_pending
        .get(&tid)
        .is_some_and(|&(at, _)| at > depth || pop_dropped.is_some_and(|p| at + 1 == p));
    let mut global = None;
    if forced_dropped {
        if let Some((_, JvmtiForced::Global(g))) = ds.early_returns.jvmti_pending.remove(&tid) {
            global = Some(g);
        }
    }
    drop(ds);
    if let Some(g) = global {
        let _ = shared.natives.jni_global_refs.lock().remove(g);
    }
    let dropped = pop_dropped.is_some() || forced_dropped;
    if dropped && crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_EARLY_RETURN] dropped stale frame control tid={tid} above depth={depth}");
    }
    dropped
}

/// Thread `tid` ends (its `ThreadDeath`; interpreter round i7 wave 136, lane
/// L1): a C agent's frame control it never took, and its parks, are dropped,
/// and a forced return's object is released. Until then the record stood for
/// the dead thread, its JNI global reference a root for the rest of the run.
/// `ds` is locked by the caller; the global reference table is taken under
/// it, as [`discard_pending`] takes it.
pub(crate) fn forget_thread(shared: &crate::vm::SharedVm, ds: &mut DebugState, tid: u64) {
    ds.early_returns.points.remove(&tid);
    ds.early_returns.jvmti_pops.remove(&tid);
    if let Some((_, JvmtiForced::Global(global))) = ds.early_returns.jvmti_pending.remove(&tid) {
        let _ = shared.natives.jni_global_refs.lock().remove(global);
    }
}

/// The record a blocked thread's window request goes into (wave 134): the
/// one standing, or a new one for the top frame of `frames`.
fn blocked_control<'a>(
    ds: &'a mut DebugState,
    tid: u64,
    frames: &[crate::runtime::frame::Frame],
) -> Option<&'a mut super::DeferredFrameControl> {
    let top = frames.last()?;
    let fresh = super::DeferredFrameControl {
        frame_count: frames.len(),
        class_id: u64::from(top.class_id.as_u32()),
        method_id: super::frame_method_id(top),
        pop: None,
        forced: None,
        forced_after_pop: false,
    };
    let pops = ds.deferred_frame_pops.entry(tid).or_default();
    Some(pops.control.get_or_insert(fresh))
}

/// A C agent's `PopFrame` of thread `tid`'s frame at `depth` was dropped
/// untaken (interpreter round i7 wave 135, lane L1;
/// `pop_frames::drop_jvmti_pop_at`): a forced return recorded after it, for
/// the popped frame's caller (`depth - 1`), is moved to `depth`, the frame
/// still on top. Answers whether one was moved.
pub(crate) fn rekey_jvmti_forced_after_dropped_pop(
    ds: &mut DebugState,
    tid: u64,
    depth: usize,
) -> bool {
    match ds.early_returns.jvmti_pending.get_mut(&tid) {
        Some(entry) if entry.0.checked_add(1) == Some(depth) => {
            entry.0 = depth;
            true
        }
        _ => false,
    }
}

/// The deferred frame control standing for thread `tid` (wave 134).
fn deferred_control(ds: &DebugState, tid: u64) -> Option<super::DeferredFrameControl> {
    ds.deferred_frame_pops.get(&tid).and_then(|pops| pops.control)
}

/// Record a C agent's `ForceEarlyReturn*` (family `kind`, as
/// [`record_jvmti`]) of the top frame of `frames`, the frames of thread
/// `tid`, suspended while blocked entering a monitor (wave 134). The caller
/// holds the thread's inspection window and has judged the top frame:
/// `top_servable` false is `OPAQUE_FRAME`. HotSpot's order
/// (`SetForceEarlyReturn::doit`): a forced return already pending
/// `INTERNAL`, then the frame `OPAQUE_FRAME`, then the value's kind or class
/// `TYPE_MISMATCH`. On success the record owns `value`'s global reference;
/// on refusal the caller still does.
pub(crate) fn defer_blocked_forced_return(
    shared: &crate::vm::SharedVm,
    tid: u64,
    frames: &[crate::runtime::frame::Frame],
    top_servable: bool,
    kind: u8,
    value: JvmtiForced,
) -> Result<(), u16> {
    let top = frames.last().ok_or(ERR_OPAQUE_FRAME)?;
    let ret = top.return_tag();
    // Read before the debug-state lock, as `record_jvmti` reads it.
    let assignable = match value {
        JvmtiForced::Global(global) => {
            let obj = shared
                .natives
                .jni_global_refs
                .lock()
                .resolve(global)
                .ok_or(ERR_INVALID_OBJECT)?;
            let descriptor = top.method_descriptor();
            let declared = descriptor.split_once(')').map_or("", |(_, r)| r);
            object_assignable(shared, obj, declared)
        }
        _ => true,
    };
    let mut ds = shared.debug.debug_state.lock();
    let standing = deferred_control(&ds, tid);
    blocked_forced_refusal(
        has_pending(&ds, tid),
        standing,
        top_servable,
        kind_fits(ret, kind) && assignable,
    )?;
    let control = blocked_control(&mut ds, tid, frames).ok_or(ERR_OPAQUE_FRAME)?;
    control.forced = Some(value);
    control.forced_after_pop = control.pop.is_some();
    shared.debug.debugger_gates.set_frame_writes_pending(true);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_EARLY_RETURN] deferred tid={tid} kind={} depth={}",
            kind as char,
            frames.len()
        );
    }
    Ok(())
}

/// [`defer_blocked_forced_return`]'s refusal, in HotSpot's order: a forced
/// return pending (at a park, or deferred) `INTERNAL`; a top frame not served
/// `OPAQUE_FRAME`; a value that does not fit `TYPE_MISMATCH`.
fn blocked_forced_refusal(
    pending: bool,
    standing: Option<super::DeferredFrameControl>,
    top_servable: bool,
    fits: bool,
) -> Result<(), u16> {
    if pending || standing.is_some_and(|c| c.forced.is_some()) {
        return Err(ERR_INTERNAL);
    }
    if !top_servable {
        return Err(ERR_OPAQUE_FRAME);
    }
    if !fits {
        return Err(ERR_TYPE_MISMATCH);
    }
    Ok(())
}

/// Record a C agent's `PopFrame` of the top frame of `frames`, the frames of
/// thread `tid`, suspended while blocked entering a monitor (wave 134).
/// `checked` is the caller's verdict on the frames
/// (`pop_frames::checked_blocked_pop`: whether the invoke that called the
/// frame is an `invokestatic`, or the refusal), made through the thread's
/// inspection window before this takes the debug-state lock. A `PopFrame`
/// already pending is `INTERNAL` first, as `UpdateForPopTopFrameClosure::doit`
/// answers it.
pub(crate) fn defer_blocked_pop(
    shared: &crate::vm::SharedVm,
    tid: u64,
    frames: &[crate::runtime::frame::Frame],
    checked: Result<bool, u16>,
) -> Result<(), u16> {
    let mut ds = shared.debug.debug_state.lock();
    if ds.early_returns.jvmti_pops.contains_key(&tid)
        || deferred_control(&ds, tid).is_some_and(|c| c.pop.is_some())
    {
        return Err(ERR_INTERNAL);
    }
    let is_static = checked?;
    let control = blocked_control(&mut ds, tid, frames).ok_or(ERR_OPAQUE_FRAME)?;
    control.pop = Some(is_static);
    shared.debug.debugger_gates.set_frame_writes_pending(true);
    drop(ds);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[POP_FRAMES] jvmti deferred tid={tid} depth={}", frames.len());
    }
    Ok(())
}

/// Where the records of a deferred frame control go when the thread wakes
/// with `depth` frames (wave 134): the pop at `depth`, the forced return at
/// `depth`, or at `depth - 1` when it was made after the pop (the popped
/// frame's caller returns it). `None` for each part not recorded.
fn adopted_depths(
    control: &super::DeferredFrameControl,
    depth: usize,
) -> (Option<(usize, bool)>, Option<usize>) {
    let pop = control.pop.map(|is_static| (depth, is_static));
    let forced = control.forced.map(|_| {
        if control.forced_after_pop && control.pop.is_some() {
            depth.saturating_sub(1)
        } else {
            depth
        }
    });
    (pop, forced)
}

/// Thread `thread` leaves the blocking region of a monitor entry with the
/// frame control `control` deferred for it (wave 134), the debug state
/// locked (`ds`): if its top frame is still the one the request named, the
/// pop and the forced return become the records a callback point leaves
/// ([`EarlyReturns::jvmti_pops`], [`EarlyReturns::jvmti_pending`]), the
/// thread's agent-request word gets `AGENT_REQUEST_FRAME_CONTROL` and this OS
/// thread's loop poll word is bumped, so the dispatch loop's next top takes
/// them before the next bytecode runs (`interpreter::take_callback_frame_control`).
/// Otherwise the request is dropped (its object released): the frame it
/// named is gone. Runs on the thread itself, between two polls.
pub(crate) fn adopt_blocked_frame_control(
    shared: &crate::vm::SharedVm,
    ds: &mut DebugState,
    thread: &crate::threading::jvm_thread::JvmThread,
    control: super::DeferredFrameControl,
) {
    let tid = thread.thread_id.0;
    let depth = thread.frames.len();
    let same_frame = depth == control.frame_count
        && thread.frames.last().is_some_and(|top| {
            u64::from(top.class_id.as_u32()) == control.class_id
                && super::frame_method_id(top) == control.method_id
        });
    if !same_frame {
        if let Some(JvmtiForced::Global(global)) = control.forced {
            let _ = shared.natives.jni_global_refs.lock().remove(global);
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[BLOCKED_FRAME_CONTROL] dropped tid={tid} depth={depth}");
        }
        return;
    }
    let (pop, forced_at) = adopted_depths(&control, depth);
    if let Some(at) = pop {
        ds.early_returns.jvmti_pops.insert(tid, at);
    }
    if let (Some(at), Some(value)) = (forced_at, control.forced) {
        ds.early_returns.jvmti_pending.insert(tid, (at, value));
    }
    thread.gc_block_state.agent_requests.fetch_or(
        crate::threading::jvm_thread::AGENT_REQUEST_FRAME_CONTROL,
        std::sync::atomic::Ordering::Relaxed,
    );
    crate::threading::gc_barrier::note_code_moved_on_this_thread();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[BLOCKED_FRAME_CONTROL] adopted tid={tid} depth={depth} pop={} forced={}",
            pop.is_some(),
            forced_at.is_some()
        );
    }
}

/// Release the object of a deferred frame control dropped without its wake
/// (wave 134; the thread is leaving the VM).
pub(crate) fn release_blocked_frame_control(
    shared: &crate::vm::SharedVm,
    control: &super::DeferredFrameControl,
) {
    if let Some(JvmtiForced::Global(global)) = control.forced {
        let _ = shared.natives.jni_global_refs.lock().remove(global);
    }
}

/// Is `obj` a value a method declaring the return type `declared` (a field
/// descriptor) may return (wave 61): by name over the object's supertypes, as
/// HotSpot's `VM_GetOrSetLocal::is_assignable` compares; an array by its
/// array class's descriptor, or as an `Object`, `Cloneable` or
/// `Serializable`, or a reference array as an array of a supertype of its
/// component.
fn object_assignable(
    shared: &crate::vm::SharedVm,
    obj: crate::types::ObjectRef,
    declared: &str,
) -> bool {
    let cm = shared.classes.class_manager.read();
    if super::inspect::is_array(shared, obj) {
        if matches!(
            declared,
            "Ljava/lang/Object;" | "Ljava/lang/Cloneable;" | "Ljava/io/Serializable;"
        ) {
            return true;
        }
        let Some((desc, component)) = super::inspect::array_descriptor(shared, &cm, obj) else {
            return false;
        };
        if desc == declared {
            return true;
        }
        let element = declared
            .strip_prefix("[L")
            .and_then(|r| r.strip_suffix(';'));
        return match (component, element) {
            (Some(component), Some(element)) => {
                desc.starts_with("[L") && cm.is_assignable_to_name(component, element)
            }
            _ => false,
        };
    }
    let Some(name) = declared.strip_prefix('L').and_then(|r| r.strip_suffix(';')) else {
        return false;
    };
    cm.is_assignable_to_name(shared.mem.heap.class_id_of(obj), name)
}

/// Interpreter round i6 wave 134, lane L1 (i133-L1,
/// `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`): the frame control of a
/// thread blocked entering a monitor is refused in HotSpot's order, recorded
/// once per kind, and adopted at the wake as a callback point's records.
#[cfg(test)]
mod i134_l1_blocked_frame_control_tests {
    use super::*;
    use crate::classloading::ClassId;
    use crate::runtime::frame::Frame;
    use crate::threading::jvm_thread::{JvmThread, ThreadId, AGENT_REQUEST_FRAME_CONTROL};
    use std::sync::atomic::Ordering;

    fn frame(name: &str) -> Frame {
        Frame::new(
            ClassId::new(0),
            "T".to_string(),
            name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    fn blocked_thread(tid: u64) -> JvmThread {
        let mut thread = JvmThread::new(ThreadId(tid), "blocked-entry");
        thread.frames.push(frame("caller"));
        thread.frames.push(frame("entry"));
        thread
    }

    fn standing(shared: &crate::vm::SharedVm, tid: u64) -> Option<crate::debug::DeferredFrameControl> {
        deferred_control(&shared.debug.debug_state.lock(), tid)
    }

    /// `INTERNAL` before `OPAQUE_FRAME` before `TYPE_MISMATCH`, as
    /// `SetForceEarlyReturn::doit` checks them.
    #[test]
    fn a_blocked_forced_return_is_refused_in_hotspots_order() {
        let forced = crate::debug::DeferredFrameControl {
            frame_count: 2,
            class_id: 0,
            method_id: 0,
            pop: None,
            forced: Some(JvmtiForced::Void),
            forced_after_pop: false,
        };
        assert_eq!(blocked_forced_refusal(true, None, false, false), Err(ERR_INTERNAL));
        assert_eq!(
            blocked_forced_refusal(false, Some(forced), false, false),
            Err(ERR_INTERNAL)
        );
        assert_eq!(blocked_forced_refusal(false, None, false, false), Err(ERR_OPAQUE_FRAME));
        assert_eq!(blocked_forced_refusal(false, None, true, false), Err(ERR_TYPE_MISMATCH));
        assert_eq!(blocked_forced_refusal(false, None, true, true), Ok(()));
        // A deferred pop alone does not refuse a forced return.
        let popped = crate::debug::DeferredFrameControl {
            pop: Some(true),
            forced: None,
            ..forced
        };
        assert_eq!(blocked_forced_refusal(false, Some(popped), true, true), Ok(()));
    }

    /// The pop goes to the wake's depth; the forced return too, or to the
    /// caller's when it was made after the pop.
    #[test]
    fn adopted_records_land_where_a_callback_points_would() {
        let mut c = crate::debug::DeferredFrameControl {
            frame_count: 3,
            class_id: 0,
            method_id: 0,
            pop: None,
            forced: Some(JvmtiForced::Void),
            forced_after_pop: false,
        };
        assert_eq!(adopted_depths(&c, 3), (None, Some(3)));
        c.pop = Some(false);
        assert_eq!(adopted_depths(&c, 3), (Some((3, false)), Some(3)));
        c.forced_after_pop = true;
        assert_eq!(adopted_depths(&c, 3), (Some((3, false)), Some(2)));
        c.forced = None;
        assert_eq!(adopted_depths(&c, 3), (Some((3, false)), None));
    }

    /// One pop and one forced return per suspension; the wake of the same
    /// frame adopts both and raises the agent-request word.
    #[test]
    fn a_blocked_request_is_recorded_once_and_adopted_at_the_wake() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = 13_401;
        let thread = blocked_thread(tid);
        assert_eq!(defer_blocked_pop(&shared, tid, &thread.frames, Ok(true)), Ok(()));
        assert_eq!(
            defer_blocked_pop(&shared, tid, &thread.frames, Ok(true)),
            Err(ERR_INTERNAL),
            "a second PopFrame before the resume"
        );
        let forced = |kind, value| {
            defer_blocked_forced_return(&shared, tid, &thread.frames, true, kind, value)
        };
        assert_eq!(
            forced(b'I', JvmtiForced::Plain(Value::Int(1))),
            Err(ERR_TYPE_MISMATCH),
            "an int for a void method"
        );
        assert_eq!(forced(b'V', JvmtiForced::Void), Ok(()));
        assert_eq!(forced(b'V', JvmtiForced::Void), Err(ERR_INTERNAL));
        let control = standing(&shared, tid).expect("recorded");
        assert_eq!(
            (control.frame_count, control.pop, control.forced_after_pop),
            (2, Some(true), true)
        );
        assert!(shared.debug.debugger_gates.frame_writes_pending());
        assert!(!jvmti_frame_control_at(&shared, tid, 2), "not before the wake");
        {
            let mut ds = shared.debug.debug_state.lock();
            let pops = ds.deferred_frame_pops.remove(&tid).expect("pops");
            adopt_blocked_frame_control(&shared, &mut ds, &thread, pops.control.expect("control"));
            assert_eq!(ds.early_returns.jvmti_pops.get(&tid), Some(&(2, true)));
            assert_eq!(ds.early_returns.jvmti_pending.get(&tid), Some(&(1, JvmtiForced::Void)));
        }
        assert!(jvmti_frame_control_at(&shared, tid, 2));
        assert_ne!(
            thread.gc_block_state.agent_requests.load(Ordering::Relaxed) & AGENT_REQUEST_FRAME_CONTROL,
            0,
            "the loop's reader is raised"
        );
    }

    /// Interpreter round i7 wave 135, lane L1: a blocked `PopFrame` the
    /// taking loop drops (its entry frame) moves the forced return made after
    /// it from the caller's depth to the frame that stays on top, instead of
    /// leaving it to stand forever.
    #[test]
    fn i135_l1_a_dropped_blocked_pop_moves_the_forced_return_to_the_top() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = 13_501;
        let thread = blocked_thread(tid);
        assert_eq!(defer_blocked_pop(&shared, tid, &thread.frames, Ok(true)), Ok(()));
        assert_eq!(
            defer_blocked_forced_return(&shared, tid, &thread.frames, true, b'V', JvmtiForced::Void),
            Ok(())
        );
        {
            let mut ds = shared.debug.debug_state.lock();
            let pops = ds.deferred_frame_pops.remove(&tid).expect("pops");
            adopt_blocked_frame_control(&shared, &mut ds, &thread, pops.control.expect("control"));
            assert_eq!(ds.early_returns.jvmti_pending.get(&tid), Some(&(1, JvmtiForced::Void)));
        }
        assert!(crate::debug::pop_frames::drop_jvmti_pop_at(&shared, tid, 2));
        let ds = shared.debug.debug_state.lock();
        assert!(ds.early_returns.jvmti_pops.get(&tid).is_none());
        assert_eq!(ds.early_returns.jvmti_pending.get(&tid), Some(&(2, JvmtiForced::Void)));
        drop(ds);
        // Nothing to move: a forced return of the top frame itself stays.
        let mut ds = shared.debug.debug_state.lock();
        assert!(!rekey_jvmti_forced_after_dropped_pop(&mut ds, tid, 2));
        assert!(!rekey_jvmti_forced_after_dropped_pop(&mut ds, 13_502, 2));
    }

    /// A wake whose top frame is not the one the request named drops it.
    #[test]
    fn a_wake_of_another_frame_drops_the_request() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = 13_402;
        let mut thread = blocked_thread(tid);
        assert_eq!(defer_blocked_pop(&shared, tid, &thread.frames, Ok(false)), Ok(()));
        let control = standing(&shared, tid).expect("recorded");
        thread.frames.push(frame("deeper"));
        let mut ds = shared.debug.debug_state.lock();
        adopt_blocked_frame_control(&shared, &mut ds, &thread, control);
        assert!(ds.early_returns.jvmti_pops.get(&tid).is_none());
        drop(ds);
        assert_eq!(
            thread.gc_block_state.agent_requests.load(Ordering::Relaxed) & AGENT_REQUEST_FRAME_CONTROL,
            0
        );
        // A refusal the caller computed is answered after the pending test.
        assert_eq!(
            defer_blocked_pop(&shared, 13_403, &thread.frames, Err(ERR_OPAQUE_FRAME)),
            Err(ERR_OPAQUE_FRAME)
        );
        assert!(standing(&shared, 13_403).is_none(), "nothing recorded on refusal");
    }

    /// Interpreter round i7 wave 136, lane L1: the target depth is the pop's
    /// when one is recorded; a stale pop drops the forced return made after
    /// it; a record at or below the depth stays.
    #[test]
    fn i136_l1_stale_frame_control_is_dropped_above_a_depth() {
        let shared = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let tid = 13_601;
        assert_eq!(jvmti_frame_control_depth(&shared, tid), None);
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.early_returns.jvmti_pops.insert(tid, (4, false));
            ds.early_returns.jvmti_pending.insert(tid, (3, JvmtiForced::Void));
        }
        assert_eq!(jvmti_frame_control_depth(&shared, tid), Some(4), "the pop's");
        assert!(!discard_jvmti_frame_control_above(&shared, tid, 4), "nothing above 4");
        assert_eq!(jvmti_frame_control_depth(&shared, tid), Some(4));
        assert!(discard_jvmti_frame_control_above(&shared, tid, 3));
        assert_eq!(
            jvmti_frame_control_depth(&shared, tid),
            None,
            "the forced return made after the dropped pop goes with it"
        );
        shared
            .debug
            .debug_state
            .lock()
            .early_returns
            .jvmti_pending
            .insert(tid, (2, JvmtiForced::Plain(Value::Int(7))));
        assert_eq!(jvmti_frame_control_depth(&shared, tid), Some(2), "the forced return's");
        assert!(!discard_jvmti_frame_control_above(&shared, tid, 2));
        assert!(discard_jvmti_frame_control_above(&shared, tid, 1));
        assert_eq!(jvmti_frame_control_depth(&shared, tid), None);
        // A thread's end drops what it never took, and its parks.
        enter_point(&shared, tid, 2, 0);
        {
            let mut ds = shared.debug.debug_state.lock();
            ds.early_returns.jvmti_pops.insert(tid, (2, true));
            ds.early_returns.jvmti_pending.insert(tid, (1, JvmtiForced::Void));
            forget_thread(&shared, &mut ds, tid);
            assert!(innermost_point(&ds, tid).is_none());
        }
        assert_eq!(jvmti_frame_control_depth(&shared, tid), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The return kinds JVMTI's `ForceEarlyReturn*` families accept.
    #[test]
    fn a_value_fits_the_return_types_of_its_kind() {
        for ret in *b"ZBCSI" {
            for tag in *b"ZBCSI" {
                assert!(kind_fits(ret, tag), "{} {}", ret as char, tag as char);
            }
            assert!(!kind_fits(ret, b'J'));
            assert!(!kind_fits(ret, b'L'));
            assert!(!kind_fits(ret, b'V'));
        }
        assert!(kind_fits(b'J', b'J') && !kind_fits(b'J', b'I') && !kind_fits(b'J', b'D'));
        assert!(kind_fits(b'F', b'F') && !kind_fits(b'F', b'D'));
        assert!(kind_fits(b'D', b'D') && !kind_fits(b'D', b'F'));
        assert!(kind_fits(b'V', b'V') && !kind_fits(b'V', b'I') && !kind_fits(b'V', b'L'));
        for tag in *b"L[stglc" {
            assert!(kind_fits(b'L', tag) && kind_fits(b'[', tag));
        }
        assert!(!kind_fits(b'L', b'I') && !kind_fits(b'[', b'V'));
    }

    /// A value is recorded for one park level and taken only there; the
    /// points nest.
    #[test]
    fn a_forced_return_is_taken_at_its_own_park() {
        let shared =
            std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        enter_point(&shared, 7, 3, 0);
        enter_point(&shared, 7, 5, 3);
        {
            let ds = shared.debug.debug_state.lock();
            let point = |depth, loop_base| Point {
                depth,
                loop_base,
                popped: false,
                reinvokable_entry: false,
            };
            assert_eq!(
                ds.early_returns.points.get(&7),
                Some(&vec![point(3, 0), point(5, 3)])
            );
            assert_eq!(innermost_point(&ds, 7), Some(point(5, 3)));
        }
        shared
            .debug
            .debug_state
            .lock()
            .early_returns
            .pending
            .insert(7, (3, DebuggerValue::Int(99)));
        assert_eq!(take(&shared, 7, 5), None, "recorded at the outer park");
        leave_point(&shared, 7);
        assert_eq!(take(&shared, 7, 3), Some(Some(Value::Int(99))));
        assert_eq!(take(&shared, 7, 3), None, "taken once");
        leave_point(&shared, 7);
        assert!(shared
            .debug
            .debug_state
            .lock()
            .early_returns
            .points
            .is_empty());
        shared
            .debug
            .debug_state
            .lock()
            .early_returns
            .pending
            .insert(8, (1, DebuggerValue::Void));
        assert_eq!(take(&shared, 8, 1), Some(None), "void");
    }
}
