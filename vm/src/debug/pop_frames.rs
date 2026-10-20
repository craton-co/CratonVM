// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP `StackFrame.PopFrames` (16/4; interpreter round i1 wave 44, lane L1,
//! stage 3 of
//! `docs/internal/fixed-bugs/interpreter-L1-proposal-pop-frames-force-early-return-and-source-debug-extension-FIXED-20261008.md`):
//! an IDE's "Drop Frame" / "Reset Frame" — pop the frames down to and
//! including the one named, so the thread stands at its caller's invoke and
//! re-runs the call when resumed.
//!
//! **Where it is served.** At the interpreter's suspend point before a
//! bytecode, the park `ForceEarlyReturn` is served at
//! (`debug::early_return::enter_point`), which also records the entry frame
//! of the dispatch loop that parked. The command runs on the parked thread
//! ([`super::run_on_parked_thread`]) and pops there, before it answers: JDI
//! reads the thread's frames right after `popFrames` and finds the caller on
//! top, at its invoke, as on HotSpot. The frames are popped as an unwind
//! pops them (their block monitors and a `synchronized` method's monitor
//! released, as JDI specifies; no event), the invoke's arguments are pushed
//! back onto the caller's operand stack from the popped frame's parameter
//! slots (so an argument the debugger or the method changed stays changed,
//! as JDI specifies), the caller's `pc` is set back to the invoke
//! (`interpreter::pop_frames_for_debugger`), and the thread's frame ids move
//! to a new generation, so the listing the park republishes names the
//! caller as frame 0 and an id of the old listing is `INVALID_FRAMEID`. When
//! the debugger resumes the thread, the suspend point answers
//! `DebuggerStop::FramesPopped` and the dispatch loop runs on in the caller
//! at the invoke.
//!
//! **Refused**: `NO_MORE_FRAMES` for a frame with no caller (the thread's
//! bottom frame: HotSpot 25.0.3's answer to popping `main`, which JDI throws
//! as `InvalidStackFrameException`, `tools/probes/interp/L1/L1W44JdiPopFrames.java`);
//! `OPAQUE_FRAME` (JDI: `NativeMethodException`) when a frame to pop or the
//! caller is a native method's, a compiled activation's or an interpreter
//! frame's whose body runs compiled; when the caller belongs to another
//! dispatch loop (a JNI upcall's, a reflective call's, a `<clinit>` run
//! under an invoke: no loop here can re-enter it); when the caller's
//! bytecode is not an `invokevirtual` / `invokespecial` / `invokestatic` /
//! `invokeinterface` naming the popped method (an `invokedynamic`, a
//! signature-polymorphic call, a frame pushed by something else); when a
//! frame runs an obsolete method, overlaps its caller's operand stack
//! (`CRATONVM_JIT_OVERLAP_ARGS`) or belongs to a virtual thread's
//! continuation; and when the thread is parked anywhere but before a
//! bytecode; `THREAD_NOT_SUSPENDED`, `INVALID_THREAD`, `INVALID_FRAMEID` as
//! for the other frame commands. A forced return recorded and not yet taken
//! is dropped with the frame it was to return from.

use super::commands::{
    thread_status_of, CommandResult, ERR_INTERNAL, ERR_INVALID_FRAMEID, ERR_INVALID_THREAD,
    ERR_NO_MORE_FRAMES, ERR_OPAQUE_FRAME, ERR_THREAD_NOT_SUSPENDED, THREAD_STATUS_ZOMBIE,
};
use super::protocol::PayloadReader;
use crate::runtime::frame::Frame;
use crate::types::Value;

/// `StackFrame.PopFrames` (16/4): `thread`, then `frame`. See the module doc.
pub(crate) fn pop_frames(shared: &crate::vm::SharedVm, data: &[u8]) -> CommandResult {
    let mut r = PayloadReader::new(data);
    let (Ok(tid), Ok(frame_id)) = (r.read_u64_be(), r.read_u64_be()) else {
        return CommandResult::error(ERR_INTERNAL);
    };
    {
        let ds = shared.debug.debug_state.lock();
        match thread_status_of(&ds, tid) {
            None | Some(THREAD_STATUS_ZOMBIE) => return CommandResult::error(ERR_INVALID_THREAD),
            Some(_) if !ds.is_thread_suspended(tid) => {
                return CommandResult::error(ERR_THREAD_NOT_SUSPENDED)
            }
            Some(_) => {}
        }
        let listed = ds
            .thread_frames
            .get(&tid)
            .is_some_and(|frames| frames.iter().any(|f| f.frame_id == frame_id));
        if !listed {
            return CommandResult::error(ERR_INVALID_FRAMEID);
        }
    }
    let popped = super::run_on_parked_thread(shared, Some(tid), move |shared, thread| {
        pop_on_thread(shared, thread, tid, frame_id)
    });
    match popped {
        Ok(Ok(())) => CommandResult::ok(Vec::new()),
        Ok(Err(code)) => {
            // The negative control of the positive one in `pop_on_thread`.
            if crate::runtime::env_cache::frame_trace() {
                eprintln!("[POP_FRAMES] refused tid={tid} error={code}");
            }
            CommandResult::error(code)
        }
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

/// Pop thread `tid`'s frames down to and including the one its listing
/// names `frame_id`, on that thread while it is parked (the JDWP answer's
/// error code on refusal). Every check runs before anything changes. Runs
/// with the suspend point suppressed and no collection possible (a
/// registered mutator between two polls), so the argument references read
/// here stay put until they are pushed back.
fn pop_on_thread(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    tid: u64,
    frame_id: u64,
) -> Result<(), u16> {
    let (target, _, args) = checked_pop(shared, thread, tid, frame_id)?;
    apply_pop(shared, thread, tid, target, &args, true);
    Ok(())
}

/// [`pop_on_thread`]'s checks, every one before anything changes: answers
/// the index of the frame to pop (the one the listing names `frame_id`),
/// whether its caller's invoke is an `invokestatic`, and the invoke's
/// arguments as the frame holds them now.
fn checked_pop(
    shared: &crate::vm::SharedVm,
    thread: &crate::threading::jvm_thread::JvmThread,
    tid: u64,
    frame_id: u64,
) -> Result<(usize, bool, Vec<Value>), u16> {
    let depth = thread.frames.len();
    // The listing and the park, under the debug-state lock.
    let (target, caller) = {
        let ds = shared.debug.debug_state.lock();
        let listing = ds.thread_frames.get(&tid).ok_or(ERR_INVALID_FRAMEID)?;
        let pos = listing
            .iter()
            .position(|e| e.frame_id == frame_id)
            .ok_or(ERR_INVALID_FRAMEID)?;
        // Every row from the top through the caller is an interpreter frame
        // with its own locals, so row `k` is frame `depth - 1 - k`.
        let plain = |e: &super::FrameEntry| {
            e.offset != super::NATIVE_FRAME_LOCATION
                && !ds.opaque_frames.contains_key(&(tid, e.frame_id))
        };
        if !listing[..=pos].iter().all(|e| plain(e)) {
            return Err(ERR_OPAQUE_FRAME);
        }
        match listing.get(pos + 1) {
            None => return Err(ERR_NO_MORE_FRAMES),
            Some(e) if !plain(e) => return Err(ERR_OPAQUE_FRAME),
            Some(_) => {}
        }
        // Only a park before a bytecode, of the frame on top (or of the
        // caller a pop in this suspension left on top).
        let point = super::early_return::innermost_point(&ds, tid).ok_or(ERR_OPAQUE_FRAME)?;
        if point.depth != depth {
            return Err(ERR_OPAQUE_FRAME);
        }
        let loop_base = point.loop_base;
        let target = depth.checked_sub(pos + 1).ok_or(ERR_INVALID_FRAMEID)?;
        let caller = target.checked_sub(1).ok_or(ERR_NO_MORE_FRAMES)?;
        // A caller of another dispatch loop: no loop here can re-enter it.
        if caller < loop_base {
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[POP_FRAMES] caller of another loop tid={tid} caller={caller} loop_base={loop_base}"
                );
            }
            return Err(ERR_OPAQUE_FRAME);
        }
        (target, caller)
    };
    let (is_static, args) = checked_frames(shared, thread, target, caller)?;
    Ok((target, is_static, args))
}

/// The checks [`checked_pop`] makes of the frames themselves, once it found
/// frame `target` to pop and its `caller` (a frame of the dispatch loop that
/// parked): whether the caller's invoke is an `invokestatic`, and the
/// invoke's arguments as `target` holds them now. Also the checks of a C
/// agent's `PopFrame` from an event callback (wave 62,
/// [`request_jvmti_pop_in_callback`]).
fn checked_frames(
    shared: &crate::vm::SharedVm,
    thread: &crate::threading::jvm_thread::JvmThread,
    target: usize,
    caller: usize,
) -> Result<(bool, Vec<Value>), u16> {
    checked_frame_slice(
        shared,
        &thread.frames,
        thread.continuation_return_adapters.is_empty(),
        target,
        caller,
    )
}

/// [`checked_frames`] of `frames` (bottom first), the frames of a thread
/// whose continuation-return adapters are `no_adapters` (interpreter round i6
/// wave 134, lane L1: also read through a blocked thread's inspection window,
/// [`checked_blocked_pop`]).
fn checked_frame_slice(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    no_adapters: bool,
    target: usize,
    caller: usize,
) -> Result<(bool, Vec<Value>), u16> {
    let depth = frames.len();
    if !no_adapters {
        return Err(ERR_OPAQUE_FRAME);
    }
    for index in caller..depth {
        let f = frames.get(index).ok_or(ERR_INVALID_FRAMEID)?;
        if f.runs_obsolete_method() || (index > caller && f.locals_overlap_caller()) {
            return Err(ERR_OPAQUE_FRAME);
        }
    }
    let caller_frame = frames.get(caller).ok_or(ERR_INVALID_FRAMEID)?;
    let callee = frames.get(target).ok_or(ERR_INVALID_FRAMEID)?;
    let at = caller_frame.last_instr_pc;
    let is_static = match caller_frame.code.get(at).copied() {
        Some(0xb8) => true,
        Some(0xb6 | 0xb7 | 0xb9) => false,
        _ => return Err(ERR_OPAQUE_FRAME),
    };
    let (Some(&hi), Some(&lo)) = (caller_frame.code.get(at + 1), caller_frame.code.get(at + 2))
    else {
        return Err(ERR_OPAQUE_FRAME);
    };
    // The invoke must name the popped method (by name and descriptor: the
    // method it selected may be an override of the one it names).
    if !invoke_names(shared, caller_frame, u16::from_be_bytes([hi, lo]), callee) {
        return Err(ERR_OPAQUE_FRAME);
    }
    let args = callee_arguments(callee, is_static).ok_or(ERR_OPAQUE_FRAME)?;
    Ok((is_static, args))
}

/// Pop thread `tid`'s frames from `target` up, `args` pushed back for the
/// caller's invoke ([`checked_pop`] checked them). `parked`: the thread is
/// parked at its innermost point, which the pop moves (a C agent's
/// `PopFrame` is taken after the park, wave 61).
fn apply_pop(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    tid: u64,
    target: usize,
    args: &[Value],
    parked: bool,
) {
    super::early_return::discard_pending(shared, &mut shared.debug.debug_state.lock(), tid, target);
    // Interpreter round i2 wave 61, lane L1: a `NotifyFramePop` request of a
    // popped frame is dropped without its `FramePop`, as HotSpot drops it
    // (`JvmtiThreadState::update_for_pop_top_frame`; MEASURED,
    // `L1W61JvmtiFrameControl`'s `pop` rows): left in place, it fired for the
    // frame the re-run invoke pushes at the same index.
    let first_popped = u32::try_from(target).unwrap_or(u32::MAX);
    thread
        .frame_pop_requests
        .retain(|&index| index < first_popped);
    let popped = crate::runtime::interpreter::pop_frames_for_debugger(shared, thread, target, args);
    if crate::runtime::env_cache::frame_trace() {
        if let Some(top) = thread.frames.last() {
            eprintln!(
                "[POP_FRAMES] tid={tid} popped={popped} top={}.{}{} at={} args={}",
                top.class_name(),
                top.method_name(),
                top.method_descriptor(),
                top.pc,
                args.len()
            );
        }
    }
    // The listing the park republishes names the caller as frame 0, under
    // ids no earlier listing used; a second pop in this suspension is judged
    // against the new frame count.
    let mut ds = shared.debug.debug_state.lock();
    if parked {
        super::early_return::note_frames_popped(&mut ds, tid, thread.frames.len());
    }
    ds.invalidate_frame_ids(tid);
}

/// A C JVMTI agent's `PopFrame` of thread `tid` (interpreter round i2 wave
/// 61, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-suspension-and-frame-control-FIXED-20261025.md`),
/// on the thread while it is parked: [`checked_pop`]'s checks of its top
/// frame (the listing's frame 0), then recorded for the resume
/// ([`take_jvmti_pop`]). As on HotSpot the frame is popped when the thread
/// resumes, so until then the thread lists it (MEASURED,
/// `L1W61JvmtiFrameControl`: the frame count and the top method are the same
/// after a `PopFrame`), and a second `PopFrame` before the resume is
/// `INTERNAL` (`UpdateForPopTopFrameClosure::doit`). The refusals are JDWP's
/// numbers, which are JVMTI's for these codes.
pub(crate) fn request_jvmti_pop(
    shared: &crate::vm::SharedVm,
    thread: &crate::threading::jvm_thread::JvmThread,
    tid: u64,
) -> Result<(), u16> {
    let frame_id = {
        let ds = shared.debug.debug_state.lock();
        if ds.early_returns.jvmti_pops.contains_key(&tid) {
            return Err(ERR_INTERNAL);
        }
        ds.thread_frames
            .get(&tid)
            .and_then(|listing| listing.first())
            .map(|e| e.frame_id)
            .ok_or(ERR_OPAQUE_FRAME)?
    };
    let depth = thread.frames.len();
    let (target, is_static, _) = checked_pop(shared, thread, tid, frame_id)?;
    if target + 1 != depth {
        return Err(ERR_OPAQUE_FRAME);
    }
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .jvmti_pops
        .insert(tid, (depth, is_static));
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[POP_FRAMES] jvmti recorded tid={tid} depth={depth}");
    }
    Ok(())
}

/// A C JVMTI agent's `PopFrame` of the calling thread `tid` inside its own
/// `Breakpoint` or `SingleStep` callback (interpreter round i2 wave 62, lane
/// L1; stage 1 of
/// `docs/internal/proposals/interpreter/i61-L1-proposal-frame-control-from-an-event-callback-20261025.md`):
/// the top frame is the Java frame the event is about, at the callback
/// point the interpreter entered for it (`early_return::enter_point`). The
/// same checks as a suspended thread's pop, read from the frames (no
/// listing is published for a running thread): a second request
/// `INTERNAL`; no callback point at this depth `OPAQUE_FRAME` (the calling
/// thread's top frame is then the native calling this, as in wave 61); a
/// top frame with no caller `NO_MORE_FRAMES`; a caller of another dispatch
/// loop, or one whose invoke does not name the popped method,
/// `OPAQUE_FRAME`. Recorded for the end of the callback, where the
/// interpreter takes it ([`take_jvmti_pop`]).
pub(crate) fn request_jvmti_pop_in_callback(
    shared: &crate::vm::SharedVm,
    thread: &crate::threading::jvm_thread::JvmThread,
    tid: u64,
) -> Result<(), u16> {
    let depth = thread.frames.len();
    let loop_base = {
        let ds = shared.debug.debug_state.lock();
        if ds.early_returns.jvmti_pops.contains_key(&tid) {
            return Err(ERR_INTERNAL);
        }
        let point = super::early_return::innermost_point(&ds, tid).ok_or(ERR_OPAQUE_FRAME)?;
        if point.depth != depth || point.popped {
            return Err(ERR_OPAQUE_FRAME);
        }
        point.loop_base
    };
    let target = depth.checked_sub(1).ok_or(ERR_OPAQUE_FRAME)?;
    let caller = target.checked_sub(1).ok_or(ERR_NO_MORE_FRAMES)?;
    if caller < loop_base {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[POP_FRAMES] callback caller of another loop tid={tid} caller={caller} loop_base={loop_base}");
        }
        return Err(ERR_OPAQUE_FRAME);
    }
    let (is_static, _) = checked_frames(shared, thread, target, caller)?;
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .jvmti_pops
        .insert(tid, (depth, is_static));
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[POP_FRAMES] jvmti callback recorded tid={tid} depth={depth}");
    }
    Ok(())
}

/// The checks of a C agent's `PopFrame` of the top frame of a thread
/// suspended while blocked entering a monitor (interpreter round i6 wave
/// 134, lane L1; `early_return::defer_blocked_pop` records it), on `frames`
/// (bottom first) read through its inspection window: a top frame with no
/// caller `NO_MORE_FRAMES`, then [`checked_frames`]' checks of the two.
/// Answers whether the invoke is an `invokestatic`. The dispatch loop that
/// will take it is not known here: one whose entry frame is the popped frame
/// drops the request at the take (`interpreter::deliver_breakpoint_if_set`).
pub(crate) fn checked_blocked_pop(
    shared: &crate::vm::SharedVm,
    frames: &[Frame],
    no_adapters: bool,
) -> Result<bool, u16> {
    let (target, caller) = blocked_pop_pair(frames.len())?;
    checked_frame_slice(shared, frames, no_adapters, target, caller)
        .map(|(is_static, _)| is_static)
}

/// The frame a blocked thread's `PopFrame` pops and its caller, of `depth`
/// frames (wave 134): the top and the one under it.
fn blocked_pop_pair(depth: usize) -> Result<(usize, usize), u16> {
    let target = depth.checked_sub(1).ok_or(ERR_OPAQUE_FRAME)?;
    let caller = target.checked_sub(1).ok_or(ERR_NO_MORE_FRAMES)?;
    Ok((target, caller))
}

/// A C agent's `PopFrame` recorded for thread `tid`'s frame at `depth` that
/// the dispatch loop taking it cannot serve, dropped (wave 134): the frame is
/// the loop's entry frame, so its caller belongs to another loop. Answers
/// whether one was dropped.
pub(crate) fn drop_jvmti_pop_at(shared: &crate::vm::SharedVm, tid: u64, depth: usize) -> bool {
    let mut ds = shared.debug.debug_state.lock();
    let dropped = ds
        .early_returns
        .jvmti_pops
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth);
    if dropped {
        ds.early_returns.jvmti_pops.remove(&tid);
    }
    drop(ds);
    if dropped && crate::runtime::env_cache::frame_trace() {
        eprintln!("[POP_FRAMES] jvmti dropped at a loop's entry frame tid={tid} depth={depth}");
    }
    dropped
}

/// The resume of thread `tid`'s park with `depth` frames: pop the frame a C
/// agent's `PopFrame` recorded there ([`request_jvmti_pop`]), its arguments
/// read now (a `SetLocal*` made meanwhile stays, as JDI specifies for
/// `PopFrames`). Answers whether it popped; the caller then runs the
/// caller's invoke again (`DebuggerStop::FramesPopped`). One lock of the
/// debug state per resumed park.
#[cold]
#[inline(never)]
pub(crate) fn take_jvmti_pop(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    tid: u64,
    depth: usize,
) -> bool {
    let recorded = {
        let mut ds = shared.debug.debug_state.lock();
        match ds.early_returns.jvmti_pops.get(&tid) {
            Some(&(at, is_static)) if at == depth => {
                ds.early_returns.jvmti_pops.remove(&tid);
                Some(is_static)
            }
            _ => None,
        }
    };
    let Some(is_static) = recorded else {
        return false;
    };
    let target = depth.wrapping_sub(1);
    let args = (thread.frames.len() == depth)
        .then(|| thread.frames.get(target))
        .flatten()
        .and_then(|callee| callee_arguments(callee, is_static));
    let Some(args) = args else {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[POP_FRAMES] jvmti dropped tid={tid} depth={depth}");
        }
        return false;
    };
    apply_pop(shared, thread, tid, target, &args, false);
    true
}

/// Does the method reference at `cp_index` of `caller`'s class name the
/// method `callee` runs (its name and descriptor)? Read from the class's
/// current constant pool: the caller runs its class's current bytecode
/// (`pop_on_thread` refuses an obsolete one).
fn invoke_names(
    shared: &crate::vm::SharedVm,
    caller: &Frame,
    cp_index: u16,
    callee: &Frame,
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.class_store.get(caller.class_id) else {
        return false;
    };
    let pool = &class.constant_pool;
    let nat = match pool.get(cp_index) {
        Some(ConstantPoolEntry::MethodReference {
            name_and_type_index,
            ..
        })
        | Some(ConstantPoolEntry::InterfaceMethodReference {
            name_and_type_index,
            ..
        }) => pool.get_name_and_type(*name_and_type_index),
        _ => None,
    };
    nat.is_some_and(|(name, descriptor)| {
        name == callee.method_name() && descriptor == callee.method_descriptor()
    })
}

/// The invoke's arguments as `callee` holds them in its parameter slots
/// now, the receiver first (not for `is_static`), each of the kind its
/// descriptor declares; `None` when a slot holds another kind (the method
/// stored something else there) or the receiver is null.
fn callee_arguments(callee: &Frame, is_static: bool) -> Option<Vec<Value>> {
    let descriptor = callee.method_descriptor();
    let params = descriptor.strip_prefix('(')?.split_once(')')?.0.as_bytes();
    let mut args = Vec::new();
    let mut slot: u16 = 0;
    if !is_static {
        let receiver = super::debugger_local(callee, 0);
        if !matches!(receiver, Value::Object(Some(_))) {
            return None;
        }
        args.push(receiver);
        slot = 1;
    }
    let mut i = 0usize;
    while i < params.len() {
        let mut end = i;
        while params.get(end) == Some(&b'[') {
            end += 1;
        }
        let kind = if end > i { b'[' } else { params[i] };
        if params.get(end) == Some(&b'L') {
            end += params[end..].iter().position(|&b| b == b';')?;
        }
        let value = super::debugger_local(callee, slot);
        let fits = match (kind, value) {
            (b'J', Value::Long(_))
            | (b'F', Value::Float(_))
            | (b'D', Value::Double(_))
            | (b'L' | b'[', Value::Object(_))
            | (b'Z' | b'B' | b'C' | b'S' | b'I', Value::Int(_)) => true,
            _ => false,
        };
        if !fits {
            return None;
        }
        args.push(value);
        slot = slot.checked_add(if matches!(kind, b'J' | b'D') { 2 } else { 1 })?;
        i = end + 1;
    }
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(descriptor: &str, locals: &[Value]) -> Frame {
        let mut f = Frame::new(
            crate::classloading::ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            descriptor.to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            8,
            &[],
        );
        let mut slot = 0u16;
        for &v in locals {
            f.set_local(slot, v);
            slot += if matches!(v, Value::Long(_) | Value::Double(_)) {
                2
            } else {
                1
            };
        }
        f
    }

    /// The arguments come back in order, of their declared kinds, with the
    /// two-slot kinds skipping their second slot.
    #[test]
    fn a_popped_frames_arguments_are_read_by_its_descriptor() {
        let f = frame(
            "(IJ[Ljava/lang/String;DZ)V",
            &[
                Value::Int(3),
                Value::Long(-4),
                Value::Object(None),
                Value::Double(2.5),
                Value::Int(1),
            ],
        );
        assert_eq!(
            callee_arguments(&f, true),
            Some(vec![
                Value::Int(3),
                Value::Long(-4),
                Value::Object(None),
                Value::Double(2.5),
                Value::Int(1),
            ])
        );
        // A static method of no parameter.
        assert_eq!(callee_arguments(&frame("()V", &[]), true), Some(Vec::new()));
        // A slot holding another kind than the descriptor's.
        let changed = frame("(I)V", &[Value::Long(9)]);
        assert_eq!(callee_arguments(&changed, true), None);
        // An instance method whose receiver slot holds null.
        let null_receiver = frame("(I)V", &[Value::Object(None), Value::Int(1)]);
        assert_eq!(callee_arguments(&null_receiver, false), None);
    }

    /// Interpreter round i6 wave 134, lane L1 (i133-L1): a blocked thread's
    /// `PopFrame` pops its top frame into the one under it; a lone frame has
    /// no caller (`NO_MORE_FRAMES`), no frame at all is `OPAQUE_FRAME`.
    #[test]
    fn a_blocked_pop_names_the_top_frame_and_its_caller() {
        assert_eq!(blocked_pop_pair(3), Ok((2, 1)));
        assert_eq!(blocked_pop_pair(2), Ok((1, 0)));
        assert_eq!(blocked_pop_pair(1), Err(ERR_NO_MORE_FRAMES));
        assert_eq!(blocked_pop_pair(0), Err(ERR_OPAQUE_FRAME));
    }
}
