// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP `ThreadReference.ForceEarlyReturn` (11/14; interpreter round i1 wave
//! 43, lane L1, stage 2 of
//! `docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md`):
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
pub(crate) fn enter_point(
    shared: &crate::vm::SharedVm,
    tid: u64,
    depth: usize,
    loop_base: usize,
) {
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
/// (wave 44): a `PopFrames` removed the frame it was to return from.
pub(crate) fn discard_pending(shared: &crate::vm::SharedVm, ds: &mut DebugState, tid: u64) {
    if let Some((_, value)) = ds.early_returns.pending.remove(&tid) {
        if let Some(id) = object_id(&value) {
            release(shared, ds, id);
        }
    }
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

/// The value a `ForceEarlyReturn` recorded for thread `tid`'s park with
/// `depth` frames, taken: `Some(None)` for a `void` method, `None` when none
/// was recorded for this park. An object is resolved now (its id's pin and
/// collection hold released); the caller pushes it onto the frame before
/// its next safepoint.
pub(crate) fn take(
    shared: &crate::vm::SharedVm,
    tid: u64,
    depth: usize,
) -> Option<Option<Value>> {
    let mut ds = shared.debug.debug_state.lock();
    if ds.early_returns.pending.is_empty() {
        return None;
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
    // as `INTERNAL` (measured, `L1W43RawJdwpObjectErrorAnswers`).
    if ds.early_returns.pending.contains_key(&tid) {
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
        let shared = std::sync::Arc::new(crate::vm::SharedVm::new(
            crate::config::VmConfig::default(),
        ));
        enter_point(&shared, 7, 3, 0);
        enter_point(&shared, 7, 5, 3);
        {
            let ds = shared.debug.debug_state.lock();
            let point = |depth, loop_base| Point {
                depth,
                loop_base,
                popped: false,
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
        assert!(shared.debug.debug_state.lock().early_returns.points.is_empty());
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
