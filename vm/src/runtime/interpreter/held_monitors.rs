// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Structured locking (JVMS §2.11.10): the monitors an interpreted frame's own
//! `monitorenter`s hold, and what the interpreter does with them.
//!
//! HotSpot's interpreter keeps a monitor block per frame and enforces the
//! structured-locking rules from it; this is CratonVM's copy of that record
//! (interpreter round i1 wave 23, lane L7; page
//! `interpreter-L7-unstructured-locking-is-not-detected`):
//!
//! * `monitorexit` releases an object only through the record of the frame
//!   that executes it. An object the frame did not enter — even one the thread
//!   owns through a CALLER's frame — is `IllegalMonitorStateException` with a
//!   null message ([`monitorexit_unrecorded`]).
//! * A return with an entry still recorded throws `IllegalMonitorStateException`
//!   at the return's bci ([`return_with_held_monitors`]); like HotSpot's
//!   `remove_activation(throw_monitor_exception = true)` it does not unlock, so
//!   a handler of the method that covers the return still sees the lock held.
//! * Exception unwinding out of a frame releases every entry still recorded
//!   and replaces the in-flight throwable with a new
//!   `IllegalMonitorStateException` ([`replace_unwound_exception_if_locked`],
//!   HotSpot's `InterpreterRuntime::new_illegal_monitor_state_exception`).
//! * Every other removal of a frame (`pop_and_recycle_frame_with_reason`)
//!   releases what the record still holds — the JVMTI `PopFrame` rule, and the
//!   safety net for the paths that leave `execute_frame_from_index` through a
//!   VM error rather than a Java throwable.
//!
//! # The record may under-report, never over-report
//!
//! Only `op_monitorenter` adds an entry, only this frame's `monitorexit` (or
//! the rules above) removes one, and entering OSR'd code clears the record
//! (`try_osr_with_backoff`: from there on the compiled body owns the frame's
//! monitors and may release them itself). A frame the interpreter RESUMES
//! mid-method — a deoptimized frame, an OSR body that exited back into its
//! frame — holds monitors compiled code acquired and no record names. So a
//! `monitorexit` that misses the record is not trusted blindly: when this
//! thread holds the object more often than every frame's record (and
//! `monitor_on_exit`) accounts for, the unaccounted acquisition is the one being
//! released, and it is released as before this record existed. Only a miss the
//! monitor table cannot explain throws. A false `IllegalMonitorStateException`
//! on javac's structured shape would be a hang (its catch-any handler covers
//! its own `monitorexit`), so every direction of doubt resolves to the
//! pre-record behaviour.
//!
//! For the same reason an entry whose object this thread does not hold as
//! often as the records claim is STALE (a relocation a remap site missed, a
//! release made on the frame's behalf) and is dropped, with a debug line,
//! instead of being reported ([`prune_stale`]).
//!
//! # Cost
//!
//! The record is an inline two-slot `SmallVec`: no allocation for up to two
//! nested block monitors in one frame, a push on `monitorenter`, a compare and
//! pop on `monitorexit`, and one `is_empty` test on each return and frame pop.
//! Everything else here is `#[cold]`.
//!
//! # GC
//!
//! The entries are object references: every per-thread root scan and remap
//! that visits `Frame::monitor_on_exit` visits the record too
//! (`memory/roots.rs` `collect_roots`, `memory/gc.rs` `update_all_roots`,
//! `gc_and_alloc.rs`'s peer, cached, safepoint deposits and
//! `apply_pointer_map_to_thread`, and `vm_exec.rs`'s blocked deposit and both
//! wake remaps), through [`HeldMonitors::as_slice`] / [`HeldMonitors::remap_with`].

use super::*;
use crate::threading::jvm_thread::ThreadId;
use smallvec::SmallVec;

/// The objects a frame's own `monitorenter`s locked and its `monitorexit`s
/// have not released, oldest first. See the module doc.
#[derive(Debug, Default)]
pub(crate) struct HeldMonitors {
    objs: SmallVec<[ObjectRef; 2]>,
}

impl HeldMonitors {
    /// An empty record (no allocation).
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self {
            objs: SmallVec::new(),
        }
    }

    /// Whether the frame holds no block monitor — the only question the hot
    /// paths (every return, every frame pop) ask.
    #[inline(always)]
    pub(crate) fn is_empty(&self) -> bool {
        self.objs.is_empty()
    }

    /// Number of recorded entries (an object entered twice counts twice).
    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.objs.len()
    }

    /// Record one `monitorenter` of `obj` by this frame.
    #[inline(always)]
    pub(crate) fn push(&mut self, obj: ObjectRef) {
        self.objs.push(obj);
    }

    /// Remove the newest entry for `obj`; `false` when the frame holds none.
    /// The newest entry is the common case (javac nests its blocks), so it is
    /// one compare and a pop.
    #[inline(always)]
    pub(crate) fn remove_newest(&mut self, obj: ObjectRef) -> bool {
        match self.objs.last() {
            Some(&top) if top == obj => {
                self.objs.pop();
                true
            }
            None => false,
            Some(_) => self.remove_newest_slow(obj),
        }
    }

    #[cold]
    #[inline(never)]
    fn remove_newest_slow(&mut self, obj: ObjectRef) -> bool {
        match self.objs.iter().rposition(|&o| o == obj) {
            Some(i) => {
                self.objs.remove(i);
                true
            }
            None => false,
        }
    }

    /// Remove the entry at `index` (no-op past the end).
    fn remove_at(&mut self, index: usize) {
        if index < self.objs.len() {
            self.objs.remove(index);
        }
    }

    /// How many entries name `obj`.
    pub(crate) fn count_of(&self, obj: ObjectRef) -> usize {
        self.objs.iter().filter(|&&o| o == obj).count()
    }

    /// The entries, oldest first — the GC root scans' view.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[ObjectRef] {
        &self.objs
    }

    /// Remove and return the newest entry.
    #[inline]
    pub(crate) fn pop(&mut self) -> Option<ObjectRef> {
        self.objs.pop()
    }

    /// Forget every entry (frame reuse; entering OSR'd code).
    #[inline]
    pub(crate) fn clear(&mut self) {
        self.objs.clear();
    }

    /// Rewrite each entry through `lookup` (old address -> new address), the
    /// GC remap twin of [`Self::as_slice`]. Each entry is looked up exactly
    /// once — the blocked-wake `fixup` chains must not be applied twice — and
    /// an entry the map does not name is left alone.
    pub(crate) fn remap_with(&mut self, mut lookup: impl FnMut(usize) -> Option<usize>) {
        for o in self.objs.iter_mut() {
            // Cast: object pointer to integer address (pointer-map key)
            let old_addr = o.as_ptr() as usize;
            if let Some(new_addr) = lookup(old_addr) {
                if new_addr != 0 {
                    // SAFETY: `new_addr` came from the collector's pointer map
                    // and is the relocated object's (non-null, aligned) header,
                    // exactly as the `monitor_on_exit` remap next to every call
                    // of this function trusts it.
                    *o = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
                }
            }
        }
    }
}

/// `IllegalMonitorStateException` with a null message, HotSpot's
/// `throw_illegal_monitor_state_exception` (an empty message is
/// `RuntimeError`'s "no message" marker).
#[inline]
pub(crate) fn imse_no_message() -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::IllegalMonitorStateException {
            message: String::new(),
        },
    ))
}

/// How many times this thread holds `obj` according to the monitor table
/// (0 when it does not own it). `from_record`: `obj` came out of a record, not
/// off the operand stack, and may be a stale address (see the module doc), so
/// it is first checked to lie inside the heap — containment only
/// (`is_heap_addr`), because the header-reading `is_object_address` can
/// reject a genuine young object, and a genuine entry read as stale loses the
/// check it exists for.
fn table_holds(shared: &SharedVm, thread: &JvmThread, obj: ObjectRef, from_record: bool) -> usize {
    // Cast: object pointer to integer address
    if from_record && shared.mem.heap.is_heap_addr(obj.as_ptr() as usize).is_none() {
        return 0;
    }
    let monitors = &shared.threads.monitors;
    if !monitors.holds(obj, thread.thread_id) {
        return 0;
    }
    usize::try_from(monitors.entry_count(obj)).unwrap_or(usize::MAX)
}

/// How many acquisitions of `obj` this thread's interpreter frames account
/// for: every frame's record, plus each synchronized frame's method monitor.
fn recorded_holds(thread: &JvmThread, obj: ObjectRef) -> usize {
    thread
        .frames
        .iter()
        .map(|f| f.held_monitors.count_of(obj) + usize::from(f.monitor_on_exit == Some(obj)))
        .sum()
}

/// Release `obj` through the table, answering a failure with the Java-visible
/// null-message `IllegalMonitorStateException` HotSpot throws. The table's own
/// text names the thread and the object's address; it is a VM diagnostic (GC
/// relocation bugs surface through it) and goes to the log, not to Java.
fn release_or_imse(shared: &SharedVm, thread_id: ThreadId, obj: ObjectRef) -> Result<(), MethodCallFailed> {
    match crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, thread_id) {
        Ok(()) => Ok(()),
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalMonitorStateException { message },
        ))) => {
            tracing::warn!(
                thread_id = ?thread_id,
                detail = %message,
                "monitorexit: the monitor table refused the release"
            );
            Err(imse_no_message())
        }
        Err(other) => Err(other),
    }
}

/// `monitorexit` of a monitor the executing frame's record names: the
/// common case, one table release. A refusal here means the table and the
/// record disagree — a VM defect (a relocation the table and the frame saw
/// differently), which [`release_or_imse`] logs at `warn` with the table's
/// text ("thread N does not own the monitor for object at 0x…").
#[inline]
pub(super) fn monitorexit_recorded(
    shared: &SharedVm,
    thread_id: ThreadId,
    obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    release_or_imse(shared, thread_id, obj)
}

/// `monitorexit` of `obj` by `frames[frame_idx]`, whose record does not name
/// it. See the module doc: released when the monitor table shows an
/// acquisition no record accounts for (a frame resumed from compiled code),
/// otherwise `IllegalMonitorStateException` with a null message and the table
/// untouched.
#[cold]
#[inline(never)]
pub(super) fn monitorexit_unrecorded(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let owned = table_holds(shared, thread, obj, false);
    let recorded = recorded_holds(thread, obj);
    if owned > recorded {
        return release_or_imse(shared, thread.thread_id, obj);
    }
    if let Some(f) = thread.frames.get(frame_idx) {
        tracing::debug!(
            class = %f.class_name(),
            method = %f.method_name(),
            pc = f.last_instr_pc,
            owned,
            recorded,
            object = ?obj.as_ptr(),
            "monitorexit of a monitor this frame did not enter (JVMS 2.11.10)"
        );
    }
    Err(imse_no_message())
}

/// Drop the entries of `frames[frame_idx]` that the monitor table does not
/// back (see the module doc), newest first.
fn prune_stale(shared: &SharedVm, thread: &mut JvmThread, frame_idx: usize) {
    let Some(len) = thread.frames.get(frame_idx).map(|f| f.held_monitors.len()) else {
        return;
    };
    let mut i = len;
    while i > 0 {
        i -= 1;
        let Some(obj) = thread.frames[frame_idx]
            .held_monitors
            .as_slice()
            .get(i)
            .copied()
        else {
            continue;
        };
        let owned = table_holds(shared, thread, obj, true);
        let recorded = recorded_holds(thread, obj);
        if owned < recorded {
            tracing::debug!(
                owned,
                recorded,
                object = ?obj.as_ptr(),
                "held-monitor record: dropping an entry the monitor table does not back"
            );
            thread.frames[frame_idx].held_monitors.remove_at(i);
        }
    }
}

/// A return is executing in `frames[frame_idx]` while its record is not
/// empty. Answers the `IllegalMonitorStateException` to throw at the return's
/// bci, or `None` when every entry was stale and the return may proceed.
#[cold]
#[inline(never)]
pub(super) fn return_with_held_monitors(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Option<MethodCallFailed> {
    prune_stale(shared, thread, frame_idx);
    let still_held = thread
        .frames
        .get(frame_idx)
        .is_some_and(|f| !f.held_monitors.is_empty());
    still_held.then(imse_no_message)
}

/// The decoded return arms' form of the return check: `Err` is the
/// `IllegalMonitorStateException` to throw at the return. One `is_empty` test
/// when the frame holds no block monitor.
#[inline(always)]
pub(super) fn check_structured_return(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
    if thread
        .frames
        .get(frame_idx)
        .is_none_or(|f| f.held_monitors.is_empty())
    {
        return Ok(());
    }
    match return_with_held_monitors(shared, thread, frame_idx) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Release, newest first, every monitor `frames[frame_idx]`'s record still
/// holds, and empty the record. Answers whether anything genuine was released.
#[cold]
#[inline(never)]
pub(super) fn release_all_recorded(shared: &SharedVm, thread: &mut JvmThread, frame_idx: usize) -> bool {
    prune_stale(shared, thread, frame_idx);
    let tid = thread.thread_id;
    let mut released = false;
    while let Some(obj) = thread
        .frames
        .get_mut(frame_idx)
        .and_then(|f| f.held_monitors.pop())
    {
        released = true;
        if let Err(e) = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, tid) {
            tracing::warn!(
                thread_id = ?tid,
                error = ?e,
                "releasing a block monitor of a frame being removed failed"
            );
        }
    }
    released
}

/// Exception unwinding is about to leave `frames[frame_idx]` with monitors
/// still recorded: release them and make a new `IllegalMonitorStateException`
/// the in-flight throwable (`native_pin_roots[pin_slot]`), as HotSpot's
/// unwinding `remove_activation` does. If the new throwable cannot be built,
/// the original one keeps propagating.
#[cold]
#[inline(never)]
pub(super) fn replace_unwound_exception_if_locked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pin_slot: usize,
) {
    if !release_all_recorded(shared, thread, frame_idx) {
        return;
    }
    // Built after the release, with the frame still on the stack (its line is
    // the top of the new trace, as in HotSpot); the frames and the pinned
    // original throwable are rooted across the allocation.
    if let MethodCallFailed::ExceptionThrown(exc) = crate::runtime::exceptions::throw_runtime_error(
        shared,
        thread,
        RuntimeError::IllegalMonitorStateException {
            message: String::new(),
        },
    ) {
        let exc = super::settle_thrown_exception(shared, thread, exc);
        if let Some(slot) = thread.native_pin_roots.get_mut(pin_slot) {
            *slot = exc;
        }
    }
}

/// Every monitor the interpreted `frames` hold, as `(frame index, object)`
/// with the index into `frames` (0 = the bottom frame), in the order HotSpot's
/// `javaVFrame::locked_monitors` reports them to `ThreadInfo` / `jstack`: the
/// innermost frame first, and within a frame the newest block monitor first
/// and the synchronized method's own monitor (entered before any block) last.
/// An object a frame entered twice appears twice, and one entered by two
/// frames appears under each, as in HotSpot.
///
/// Stage 1 of proposal `i23-L7-proposal-per-frame-locked-monitors`
/// (interpreter round i1 wave 24, lane L7). Frames the interpreter resumed
/// from compiled code may under-report (see the module doc); a caller keeps
/// what the monitor table says the thread owns beyond this list. Empty (no
/// allocation) when no frame holds a monitor.
///
/// Stable contract (wave 25): the JVMTI / JDWP owned-monitor functions build on
/// this and [`attribute_locked_monitors`]; see the proposal's "Wave 25 note"
/// for what they must add (one entry per object, depth against the reported
/// frames). Change the signature or the order only together with that note.
pub(crate) fn frame_locked_monitors(frames: &[Frame]) -> Vec<(usize, ObjectRef)> {
    let mut out = Vec::new();
    for (index, frame) in frames.iter().enumerate().rev() {
        if frame.held_monitors.is_empty() && frame.monitor_on_exit.is_none() {
            continue;
        }
        out.extend(frame.held_monitors.as_slice().iter().rev().map(|&o| (index, o)));
        if let Some(o) = frame.monitor_on_exit {
            out.push((index, o));
        }
    }
    out
}

/// Attribute the monitors a thread owns (`owned`, the JMX lock stack: one
/// entry per object) to stack-trace depths, for `ThreadInfo.getLockedMonitors`.
///
/// `frame_monitors` is [`frame_locked_monitors`] of the frames `trace_len`
/// stack-trace entries were captured from (index 0 = bottom), or `None` when
/// no attribution matching that trace exists. `waiting` is the monitor the
/// thread is parked in `Object.wait` on: it is released while the thread
/// waits, and HotSpot reports it as the thread's lock, never as locked.
///
/// Answers the monitors to report and a parallel depth for each (`0` = the
/// innermost trace entry, `-1` = held but attributed to no interpreted frame:
/// compiled code, JNI). With no attribution the depth list is empty, which
/// the JMX builder reads as "every monitor at the innermost frame" (the
/// behaviour before wave 24).
pub(crate) fn attribute_locked_monitors(
    trace_len: usize,
    owned: Vec<ObjectRef>,
    frame_monitors: Option<Vec<(usize, ObjectRef)>>,
    waiting: Option<ObjectRef>,
) -> (Vec<ObjectRef>, Vec<i32>) {
    let owned: Vec<ObjectRef> = owned.into_iter().filter(|&o| Some(o) != waiting).collect();
    let Some(frame_monitors) = frame_monitors else {
        return (owned, Vec::new());
    };
    let mut monitors = Vec::with_capacity(frame_monitors.len() + owned.len());
    let mut depths = Vec::with_capacity(frame_monitors.len() + owned.len());
    for (index, obj) in frame_monitors {
        if Some(obj) == waiting || !owned.contains(&obj) || index >= trace_len {
            continue;
        }
        monitors.push(obj);
        depths.push(i32::try_from(trace_len - 1 - index).unwrap_or(i32::MAX));
    }
    for obj in owned {
        if !monitors.contains(&obj) {
            monitors.push(obj);
            depths.push(-1);
        }
    }
    (monitors, depths)
}

#[cfg(test)]
mod tests {
    use super::{attribute_locked_monitors, HeldMonitors};
    use crate::types::ObjectRef;

    fn obj(addr: usize) -> ObjectRef {
        // SAFETY: never dereferenced; the record only compares and remaps.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    #[test]
    fn two_entries_stay_inline_and_a_third_spills() {
        let mut h = HeldMonitors::new();
        assert!(h.is_empty());
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        assert!(!h.objs.spilled(), "0-2 monitors must not allocate");
        h.push(obj(0x3000));
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn remove_newest_takes_the_newest_matching_entry() {
        let mut h = HeldMonitors::new();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        h.push(obj(0x1000));
        assert!(h.remove_newest(obj(0x2000)));
        assert_eq!(h.as_slice(), &[obj(0x1000), obj(0x1000)]);
        assert!(h.remove_newest(obj(0x1000)));
        assert_eq!(h.count_of(obj(0x1000)), 1);
        assert!(!h.remove_newest(obj(0x4000)), "an object the frame never entered");
        assert!(h.remove_newest(obj(0x1000)));
        assert!(h.is_empty());
        assert!(!h.remove_newest(obj(0x1000)));
    }

    #[test]
    fn remap_rewrites_each_entry_once() {
        let mut h = HeldMonitors::new();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        // A chained map (0x1000 -> 0x2000 -> 0x3000) must not move an entry
        // twice: 0x1000 lands on 0x2000, and 0x2000 on 0x3000.
        h.remap_with(|a| match a {
            0x1000 => Some(0x2000),
            0x2000 => Some(0x3000),
            _ => None,
        });
        assert_eq!(h.as_slice(), &[obj(0x2000), obj(0x3000)]);
        // A null target is never installed.
        h.remap_with(|_| Some(0));
        assert_eq!(h.as_slice(), &[obj(0x2000), obj(0x3000)]);
    }

    /// Wave 24 (L7): the `L7W24LockedMonitorDepths` shape -- `outer` (a
    /// synchronized method on A with a block on B), `middle` (block on C),
    /// `inner` (block re-entering A) -- reported innermost first, A twice,
    /// B before the method monitor A in `outer`; the waited-on monitor and a
    /// frame entry the lock stack does not back are dropped; an owned monitor
    /// no frame names is kept at depth -1.
    #[test]
    fn locked_monitors_are_attributed_innermost_first() {
        let (a, b, c, w, jit) = (obj(0x10), obj(0x20), obj(0x30), obj(0x40), obj(0x50));
        // frames: 0 = main, 1 = outer, 2 = middle, 3 = inner, 4 = leaf
        let frames = vec![(3, a), (2, c), (1, b), (1, a), (2, w), (0, obj(0x60))];
        let owned = vec![a, b, c, w, jit];
        let (monitors, depths) = attribute_locked_monitors(5, owned, Some(frames), Some(w));
        assert_eq!(monitors, vec![a, c, b, a, jit]);
        assert_eq!(depths, vec![1, 2, 3, 3, -1]);
    }

    #[test]
    fn without_attribution_only_the_waited_monitor_is_dropped() {
        let (a, w) = (obj(0x10), obj(0x40));
        let (monitors, depths) = attribute_locked_monitors(3, vec![a, w], None, Some(w));
        assert_eq!(monitors, vec![a]);
        assert!(depths.is_empty(), "no attribution: the JMX builder's old default");
    }

    #[test]
    fn pop_and_clear() {
        let mut h = HeldMonitors::new();
        h.push(obj(0x1000));
        h.push(obj(0x2000));
        assert_eq!(h.pop(), Some(obj(0x2000)));
        h.clear();
        assert!(h.is_empty());
        assert_eq!(h.pop(), None);
    }
}
