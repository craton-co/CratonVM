// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDWP `StackFrame.PopFrames` (16/4; interpreter round i1 wave 44, lane L1,
//! stage 3 of
//! `docs/feature-designs/jvmti-delivery-threading.md`):
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
    // Interpreter round i7 wave 139, lane L1 (`CRATONVM_JDWP_LAMBDA_TARGET_POP`):
    // the entry frame of the lambda door's loop, whose caller is the spun
    // proxy's row.
    if let Some(answer) = try_door_entry_pop(shared, thread, tid, frame_id) {
        return answer;
    }
    let (target, _, args) = checked_pop(shared, thread, tid, frame_id)?;
    apply_pop(shared, thread, tid, target, &args, true);
    Ok(())
}

thread_local! {
    /// The frame count the lambda door's entry frame makes, while the door
    /// runs it (0 for none; interpreter round i7 wave 139, lane L1,
    /// [`DoorEntryScope`]). Call-scoped, set and restored around one door's
    /// run on this OS thread; not compatibility state.
    static DOOR_ENTRY_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Whether that door is the JIT helpers' interpreted-callee door, whose
    /// caller is a compiled activation, not the proxy's row
    /// ([`DoorEntryScope::enter_compiled_caller`]).
    static DOOR_ENTRY_COMPILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A popped door entry frame to push again when the park resumes
    /// ([`take_door_restart`]).
    static DOOR_RESTART: std::cell::RefCell<Option<DoorRestart>> =
        const { std::cell::RefCell::new(None) };
}

/// A door entry frame [`try_door_entry_pop`] popped: the frame count it
/// made, its method, and its arguments, each reference as an index into
/// `JvmThread::native_pin_roots` from `pin_base` (kept current by every
/// collection while the thread stays parked).
struct DoorRestart {
    depth: usize,
    cached: std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    pin_base: usize,
    args: Vec<Value>,
}

/// Marks the frame a lambda door is about to push and run as the entry frame
/// of a door that stands for the spun proxy's method (interpreter round i7
/// wave 139, lane L1; `VmConfig::jdwp_lambda_target_pop`). While the scope
/// lives, a JDWP `PopFrames` of that frame is served by
/// [`try_door_entry_pop`]. Restores the previous mark on drop (a nested door
/// marks its own; a yield's unwinding drops it).
pub(crate) struct DoorEntryScope(usize, bool);

impl DoorEntryScope {
    /// Mark the frame that will make the thread's frame count `depth`.
    #[inline]
    pub(crate) fn enter(depth: usize) -> Self {
        Self::enter_kind(depth, false)
    }

    /// [`Self::enter`] for the JIT helpers' interpreted-callee door
    /// (`interpreter::install_and_run_cached_frame_from_compiled`): the
    /// frame's caller is a compiled activation, which HotSpot deoptimizes for
    /// the pop and whose invoke it re-runs; the door calls the same resolved
    /// callee again.
    #[inline]
    pub(crate) fn enter_compiled_caller(depth: usize) -> Self {
        Self::enter_kind(depth, true)
    }

    fn enter_kind(depth: usize, compiled: bool) -> Self {
        Self(
            DOOR_ENTRY_DEPTH.with(|c| c.replace(depth)),
            DOOR_ENTRY_COMPILED.with(|c| c.replace(compiled)),
        )
    }
}

impl Drop for DoorEntryScope {
    #[inline]
    fn drop(&mut self) {
        let (depth, compiled) = (self.0, self.1);
        DOOR_ENTRY_DEPTH.with(|c| c.set(depth));
        DOOR_ENTRY_COMPILED.with(|c| c.set(compiled));
    }
}

/// JDWP `PopFrames` down to and including the lambda door's entry frame
/// (interpreter round i7 wave 139, lane L1;
/// `docs/feature-designs/jvmti-delivery-threading.md`).
/// On HotSpot the frame's caller is the spun proxy's method, a Java frame:
/// the pop leaves the proxy on top at its invoke, and the resume re-runs it,
/// calling the target again with the arguments the popped frame's parameter
/// slots held (MEASURED, `L1W138JdiLambdaProxyFrameCommands` row `pop
/// target`). Here the frame is the entry frame of the door's own loop, so its
/// caller is no frame of that loop: the frames are popped as a pop pops them
/// (monitors released, no event, a `NotifyFramePop` of them dropped), the
/// arguments are pinned, and the park's resume pushes the method again
/// ([`take_door_restart`]), which is what re-running the proxy's forwarding
/// invoke does. Meanwhile the listing has the proxy's row on top.
///
/// `None` when the request is no such pop (the switch off, no door scope,
/// another frame named, no proxy row under it): the ordinary checks answer.
/// The refusals of an admitted shape are the ordinary ones' codes.
fn try_door_entry_pop(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    tid: u64,
    frame_id: u64,
) -> Option<Result<(), u16>> {
    let enabled = if DOOR_ENTRY_COMPILED.with(std::cell::Cell::get) {
        shared.config.jdwp_compiled_caller_pop
    } else {
        shared.config.jdwp_lambda_target_pop
    };
    if !enabled {
        return None;
    }
    let door_depth = DOOR_ENTRY_DEPTH.with(std::cell::Cell::get);
    if door_depth == 0 {
        return None;
    }
    // Wave 143 (L1, review): one door pop per park. After it the door's entry
    // frame is gone; a frame a debugger invocation then pushes at the same
    // index, over the proxy's row, is no door entry frame (popping it and
    // pushing the door's method again would run the wrong method).
    if DOOR_RESTART.with(|r| r.borrow().is_some()) {
        return Some(Err(ERR_OPAQUE_FRAME));
    }
    let depth = thread.frames.len();
    let target = door_depth - 1;
    {
        let ds = shared.debug.debug_state.lock();
        let listing = ds.thread_frames.get(&tid)?;
        let pos = listing.iter().position(|e| e.frame_id == frame_id)?;
        let plain = |e: &super::FrameEntry| {
            e.offset != super::NATIVE_FRAME_LOCATION
                && !ds.opaque_frames.contains_key(&(tid, e.frame_id))
                && !ds.proxy_frames.contains(&(tid, e.frame_id))
        };
        // The named frame is the door's entry frame, every row above it a
        // frame of its loop, and the row under it the proxy's -- or, for the
        // JIT helpers' door, a compiled activation's.
        let compiled = DOOR_ENTRY_COMPILED.with(std::cell::Cell::get);
        let caller_fits = |e: &super::FrameEntry| {
            if compiled {
                ds.opaque_frames.get(&(tid, e.frame_id)) == Some(&true)
                    && !ds.proxy_frames.contains(&(tid, e.frame_id))
                    && e.offset != super::NATIVE_FRAME_LOCATION
            } else {
                ds.proxy_frames.contains(&(tid, e.frame_id))
            }
        };
        if depth.checked_sub(pos + 1) != Some(target)
            || !listing[..=pos].iter().all(|e| plain(e))
            || !listing.get(pos + 1).is_some_and(|e| caller_fits(e))
        {
            return None;
        }
        // Only a park before a bytecode of this loop.
        let point = super::early_return::innermost_point(&ds, tid);
        if !point.is_some_and(|p| p.depth == depth && !p.popped && p.loop_base == target) {
            return Some(Err(ERR_OPAQUE_FRAME));
        }
    }
    if !thread.continuation_return_adapters.is_empty() {
        return Some(Err(ERR_OPAQUE_FRAME));
    }
    if (target..depth).any(|i| thread.frames.get(i).is_none_or(|f| f.runs_obsolete_method())) {
        return Some(Err(ERR_OPAQUE_FRAME));
    }
    let callee = thread.frames.get(target)?;
    // Wave 144 (L1): a class initializer the call ran first, at the same
    // index, is no door entry frame.
    if callee.method_name() == "<clinit>" {
        return Some(Err(ERR_OPAQUE_FRAME));
    }
    // Wave 144 (L1): a frame the by-name invoke built (no template, the
    // target's first calls) gets the template its method has, as the cached
    // door would have used.
    let cached = match callee.cached_method().cloned() {
        Some(cached) => cached,
        None => match crate::runtime::interpreter::build_bytecode_callee_cached(
            shared,
            callee.class_id,
            callee.method_name(),
            callee.method_descriptor(),
            false,
        ) {
            Some((cached, _gate)) => cached,
            None => return Some(Err(ERR_OPAQUE_FRAME)),
        },
    };
    // Its monitor would have to be taken again by the re-push.
    if cached.is_synchronized {
        return Some(Err(ERR_OPAQUE_FRAME));
    }
    let Some(args) = callee_arguments(callee, cached.is_static) else {
        return Some(Err(ERR_OPAQUE_FRAME));
    };
    super::early_return::discard_pending(shared, &mut shared.debug.debug_state.lock(), tid, target);
    // Top first, each the top frame when popped.
    while thread.frames.len() > target {
        let top = thread.frames.len() - 1;
        crate::runtime::interpreter::pop_entry_frame_for_reinvoke(shared, thread, top);
    }
    let pin_base = thread.native_pin_roots.len();
    for value in &args {
        if let Value::Object(Some(object)) = value {
            thread.native_pin_roots.push(*object);
        }
    }
    DOOR_RESTART.with(|r| {
        *r.borrow_mut() = Some(DoorRestart {
            depth: door_depth,
            cached,
            pin_base,
            args,
        })
    });
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[POP_FRAMES] tid={tid} lambda door entry popped depth={door_depth}");
    }
    let mut ds = shared.debug.debug_state.lock();
    super::early_return::note_frames_popped(&mut ds, tid, thread.frames.len());
    ds.invalidate_frame_ids(tid);
    Some(Ok(()))
}

/// The door entry frame [`try_door_entry_pop`] popped while the thread was
/// parked with `depth` frames on top of it gone, to push again now that the
/// park resumes: its method and its arguments, each reference at its current
/// address (the pins are dropped). `None` when none was popped, or when the
/// thread's frame count is not the one the pop left (the record is dropped).
pub(crate) fn take_door_restart(
    thread: &mut crate::threading::jvm_thread::JvmThread,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    Vec<Value>,
)> {
    // Interpreter round i7 wave 143, lane L1 (review): only the park the pop
    // was made at takes the record. A debugger invocation run on the thread
    // after the pop (its frames above the popped ones) may park at a
    // breakpoint of its own and resume through this function first: taking
    // the record there dropped it -- and truncated the pins to the record's
    // base, under the invocation's own -- so the outer park resumed with its
    // loop's entry frame gone and nothing to push.
    let current = DOOR_RESTART.with(|r| {
        r.borrow()
            .as_ref()
            .is_some_and(|restart| thread.frames.len() + 1 == restart.depth)
    });
    if !current {
        return None;
    }
    let restart = DOOR_RESTART.with(|r| r.borrow_mut().take())?;
    let pins = &thread.native_pin_roots;
    let mut at = restart.pin_base;
    let args: Vec<Value> = restart
        .args
        .iter()
        .map(|value| match value {
            Value::Object(Some(object)) => {
                let moved = pins.get(at).copied().unwrap_or(*object);
                at += 1;
                Value::Object(Some(moved))
            }
            other => *other,
        })
        .collect();
    thread.native_pin_roots.truncate(restart.pin_base);
    Some((restart.cached, args))
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
        // Interpreter round i7 wave 138, lane L1: a lambda proxy method's row
        // has no `Frame` either (`DebugState::proxy_frames`); a pop through
        // it (or of its callee, whose invoke it is) is not served yet
        // (`interpreter-i138-L1-jdwp-lambda-proxy-rows-break-frame-commands-FIXED-20261021.md`).
        let plain = |e: &super::FrameEntry| {
            e.offset != super::NATIVE_FRAME_LOCATION
                && !ds.opaque_frames.contains_key(&(tid, e.frame_id))
                && !ds.proxy_frames.contains(&(tid, e.frame_id))
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
/// `docs/feature-designs/jvmti-delivery-threading.md`),
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
/// `docs/feature-designs/jvmti-delivery-threading.md`):
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
    let (loop_base, reinvokable_entry) = {
        let ds = shared.debug.debug_state.lock();
        if ds.early_returns.jvmti_pops.contains_key(&tid) {
            return Err(ERR_INTERNAL);
        }
        let point = super::early_return::innermost_point(&ds, tid).ok_or(ERR_OPAQUE_FRAME)?;
        if point.depth != depth || point.popped {
            return Err(ERR_OPAQUE_FRAME);
        }
        (point.loop_base, point.reinvokable_entry)
    };
    let target = depth.checked_sub(1).ok_or(ERR_OPAQUE_FRAME)?;
    let caller = target.checked_sub(1).ok_or(ERR_NO_MORE_FRAMES)?;
    // Interpreter round i7 wave 137, lane L1 (`CRATONVM_JVMTI_LAMBDA_ENTRY_POP`):
    // the entry frame of a re-invoking door (the lambda door, standing for
    // the spun proxy's method, a Java frame on HotSpot): the door pops it and
    // calls the target again ([`take_reinvokable_entry_pop`]), so no invoke
    // of the caller is checked. Refused as for any pop when the frame runs an
    // obsolete method or belongs to a virtual thread's continuation.
    if reinvokable_entry && target == loop_base {
        let frame = thread.frames.get(target).ok_or(ERR_OPAQUE_FRAME)?;
        if !thread.continuation_return_adapters.is_empty() || frame.runs_obsolete_method() {
            return Err(ERR_OPAQUE_FRAME);
        }
        shared
            .debug
            .debug_state
            .lock()
            .early_returns
            .jvmti_pops
            .insert(tid, (depth, false));
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[POP_FRAMES] jvmti callback recorded at a re-invoking door's entry tid={tid} depth={depth}");
        }
        return Ok(());
    }
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
///
/// Interpreter round i7 wave 135, lane L1: a `ForceEarlyReturn*` made after
/// that `PopFrame` was recorded for the popped frame's caller (`depth - 1`,
/// `early_return::adopt_blocked_frame_control`), which this loop never
/// reaches: left there it stood forever (every later `ForceEarlyReturn*` of
/// the thread `INTERNAL`, and a stale return taken at some later stop of the
/// caller). It is moved to the frame that stays on top, the one whose return
/// type it was checked against, as HotSpot applies a forced return to the
/// top frame when the `PopFrame` before it was refused
/// (`early_return::rekey_jvmti_forced_after_dropped_pop`).
pub(crate) fn drop_jvmti_pop_at(shared: &crate::vm::SharedVm, tid: u64, depth: usize) -> bool {
    let mut ds = shared.debug.debug_state.lock();
    let dropped = ds
        .early_returns
        .jvmti_pops
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth);
    if dropped {
        ds.early_returns.jvmti_pops.remove(&tid);
        super::early_return::rekey_jvmti_forced_after_dropped_pop(&mut ds, tid, depth);
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

/// What a re-invoking door does after its entry frame's `MethodEntry`
/// callback (interpreter round i7 wave 137, lane L1; [`take_reinvokable_entry_pop`]).
#[derive(Debug)]
pub(crate) enum ReinvokeStep {
    /// Nothing popped the frame: run it.
    Run,
    /// The frame was popped: call the target again with these arguments (the
    /// receiver first unless static), read from its parameter slots, so a
    /// `SetLocal*` the callback made stays.
    Reinvoke(Vec<Value>),
    /// The frame was popped and a `ForceEarlyReturn*` made after the
    /// `PopFrame` is the caller's (the door's, standing for the proxy's
    /// method) to return: answer this value (`None` for `void`).
    Return(Option<Value>),
}

/// A C agent's `PopFrame` of thread `tid`'s frame at `depth`, the entry frame
/// of a re-invoking door, recorded by its `MethodEntry` callback at a
/// [`super::early_return::Point::reinvokable_entry`] point (interpreter round
/// i7 wave 137, lane L1, `CRATONVM_JVMTI_LAMBDA_ENTRY_POP`): taken by the
/// door before the frame runs. The frame is popped as a `PopFrame` pops one
/// (`interpreter::pop_entry_frame_for_reinvoke`: no event, monitors
/// released), and the record's agent-request bit is cleared. A forced return
/// recorded after the pop (for `depth - 1`, `early_return::record_jvmti`) is
/// answered as the door's value, as HotSpot returns it from the proxy's frame.
/// Arguments that no longer fit the descriptor (a `SetLocal*` of another
/// kind, a null receiver) drop the request and the frame runs, as a
/// suspended thread's pop is dropped for them ([`take_jvmti_pop`]). One lock
/// of the debug state when nothing is recorded.
pub(crate) fn take_reinvokable_entry_pop(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    depth: usize,
    is_static: bool,
) -> ReinvokeStep {
    let tid = thread.thread_id.0;
    let recorded = shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .jvmti_pops
        .get(&tid)
        .is_some_and(|&(at, _)| at == depth);
    if !recorded || thread.frames.len() != depth {
        return ReinvokeStep::Run;
    }
    let target = depth - 1;
    let args = thread
        .frames
        .get(target)
        .and_then(|callee| callee_arguments(callee, is_static));
    shared
        .debug
        .debug_state
        .lock()
        .early_returns
        .jvmti_pops
        .remove(&tid);
    let Some(args) = args else {
        // A forced return made after the pop (for the caller) was conditional
        // on it; one made before it (for this frame) stays, for the loop's
        // first top. The agent-request bit is cleared when nothing is left.
        if super::early_return::jvmti_pending_at(shared, tid, target) {
            let _ = super::early_return::take(shared, tid, target);
        }
        crate::runtime::interpreter::drop_frame_control_of_unrun_frames(shared, thread, depth);
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[POP_FRAMES] jvmti re-invoking door entry dropped tid={tid} depth={depth}");
        }
        return ReinvokeStep::Run;
    };
    // A `NotifyFramePop` of the frame goes with it; nothing is pushed back.
    crate::runtime::interpreter::pop_entry_frame_for_reinvoke(shared, thread, target);
    shared.debug.debug_state.lock().invalidate_frame_ids(tid);
    let forced = if super::early_return::jvmti_pending_at(shared, tid, target) {
        super::early_return::take(shared, tid, target)
    } else {
        None
    };
    // Clears the agent-request bit the callback raised (nothing above the
    // caller is recorded now).
    crate::runtime::interpreter::drop_frame_control_of_unrun_frames(shared, thread, target);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[POP_FRAMES] jvmti re-invoking door entry popped tid={tid} depth={depth} forced={}",
            forced.is_some()
        );
    }
    match forced {
        Some(value) => ReinvokeStep::Return(value),
        None => ReinvokeStep::Reinvoke(args),
    }
}

/// The checks a compiled caller's re-invoking door makes of a `PopFrame`
/// its callee's `MethodEntry` callback recorded at `depth`, before
/// [`take_reinvokable_entry_pop`] takes it (interpreter round i7 wave 138,
/// lane L1; `interpreter::install_and_run_cached_frame_from_compiled`,
/// `CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP`). The door cannot make its
/// compiled caller return, nor dispatch the call again, so it drops the pop
/// untaken ([`drop_jvmti_pop_at`]; the frame then runs) when
///
/// * a `ForceEarlyReturn*` was made after it in the same callback (recorded
///   for the compiled caller, `depth - 1`): it is moved to the callee, as
///   HotSpot applies a forced return to the top frame when the pop before it
///   did not happen (`early_return::rekey_jvmti_forced_after_dropped_pop`);
/// * the receiver its slot 0 holds now is of another class than
///   `receiver_class`, the call's (a `SetLocalObject` made in the callback):
///   HotSpot's re-run invoke would dispatch on it.
///
/// Answers whether it dropped one. One lock of the debug state when nothing
/// is recorded.
pub(crate) fn keep_compiled_caller_entry_pop(
    shared: &crate::vm::SharedVm,
    thread: &crate::threading::jvm_thread::JvmThread,
    depth: usize,
    receiver_class: Option<crate::classloading::ClassId>,
) -> bool {
    let tid = thread.thread_id.0;
    let (recorded, forced_for_caller) = {
        let ds = shared.debug.debug_state.lock();
        let recorded = ds
            .early_returns
            .jvmti_pops
            .get(&tid)
            .is_some_and(|&(at, _)| at == depth);
        let forced = recorded
            && ds
                .early_returns
                .jvmti_pending
                .get(&tid)
                .is_some_and(|&(at, _)| at.checked_add(1) == Some(depth));
        (recorded, forced)
    };
    if !recorded || thread.frames.len() != depth {
        return false;
    }
    let receiver_moved_class = receiver_class.is_some_and(|class| {
        match depth
            .checked_sub(1)
            .and_then(|target| thread.frames.get(target))
            .and_then(|callee| callee_arguments(callee, false))
            .and_then(|args| args.first().copied())
        {
            Some(Value::Object(Some(receiver))) => shared.mem.heap.class_id_of(receiver) != class,
            // A null or unreadable receiver: `take_reinvokable_entry_pop`
            // drops the request itself.
            _ => false,
        }
    });
    if !forced_for_caller && !receiver_moved_class {
        return false;
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[POP_FRAMES] jvmti compiled caller's entry pop dropped tid={tid} depth={depth} forced_after={forced_for_caller} receiver_class_changed={receiver_moved_class}"
        );
    }
    drop_jvmti_pop_at(shared, tid, depth)
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

/// Interpreter round i7 wave 139, lane L1: the lambda door's entry mark.
#[cfg(test)]
mod i139_l1_door_entry_tests {
    use super::{DoorEntryScope, DOOR_ENTRY_DEPTH};

    fn mark() -> usize {
        DOOR_ENTRY_DEPTH.with(std::cell::Cell::get)
    }

    #[test]
    fn the_door_mark_names_the_innermost_door_and_nested_scopes_restore_it() {
        assert_eq!(mark(), 0);
        {
            let _outer = DoorEntryScope::enter(4);
            assert_eq!(mark(), 4);
            {
                let _inner = DoorEntryScope::enter(9);
                assert_eq!(mark(), 9);
            }
            assert_eq!(mark(), 4);
        }
        assert_eq!(mark(), 0);
    }

    #[test]
    fn the_loop_pushes_the_door_method_again_before_the_ordinary_pop_answer() {
        let src = include_str!("../runtime/interpreter.rs").replace("\r\n", "\n");
        let restart = src.find("crate::debug::pop_frames::take_door_restart(thread)");
        let popped = src.find("if thread.frames.len() < depth {\n                return Some(DebuggerStop::FramesPopped);");
        assert!(restart.is_some() && popped.is_some() && restart < popped);
    }
}
