// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JVMTI event delivery from the interpreter.
//!
//! The seven `fire_jvmti_*` sites plus `push_frame_and_fire_entry`, which
//! is the one that has to fire *and* push in the right order — an agent
//! that receives MethodEntry before the frame exists sees a stack that
//! does not contain the method it was told about.
//!
//! Worth collecting for a documented reason: a JVMTI delivery lane's
//! handover census said ~15 call sites in `interpreter.rs`. The real
//! count was 28, and 8 of them were in `invoke.rs` — a file the census
//! never mentioned. Trusting it would have left the cached-invoke and OSR
//! surface unattributed while looking complete. Delivery sites are now
//! countable by opening one file.

use super::site_cache::site_stats;
use super::*;

// ---------------------------------------------------------------------------
// JVMTI ExceptionCatch hook
// ---------------------------------------------------------------------------

/// Fire the JVMTI `ExceptionCatch` event when an exception-table lookup
/// resolves to a matching handler. One atomic load + branch when no agent is
/// attached anywhere: the process-wide union flag is tested here, before the
/// method id is hashed. `fire_exception_catch_for_vm` itself has no fast-path
/// gate — it takes the environments `RwLock`, looks the VM up and then takes
/// the manager's event-set lock — and there is no per-event union mirror for
/// `ExceptionCatch`, so without this guard every caught exception in every
/// VM paid all of that with no agent loaded.
///
/// The `MethodId` is [`jvmti_method_id`]'s: the real `(class_id,
/// method_index)` id when the VM can be reached.
///
/// `vm` is the raising VM's `SharedVm::vm_identity`. It is a parameter rather
/// than something read off `frame`/`thread` because neither `Frame` nor
/// `JvmThread` carries a VM identity — see the module note on
/// `push_frame_and_fire_entry`. Every caller has `shared: &SharedVm` in
/// scope, so the value is always exact and never the unattributed seam.
///
/// `thread_id` is the catching thread's (`JvmThread::thread_id`). It used to
/// be a constant `0`, so a per-thread `SetEventNotificationMode(ENABLE,
/// EXCEPTION_CATCH, thread)` could never match and the callback could not
/// tell which thread caught.
///
/// gci i11/ja (item 1b of
/// `gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`):
/// takes the catching thread's `&mut` and the handler frame's index (it took
/// the id and a `&Frame`), so the delivery runs under
/// [`post_on_own_thread`].
#[inline]
pub(super) fn fire_jvmti_exception_catch(
    vm: usize,
    thread: &mut JvmThread,
    frame_idx: usize,
    handler_pc: usize,
) {
    // Over-approximating union guard: `any_listener` is raised by every
    // event-mode enable, callback install and env attach on any VM, so it is
    // true whenever an ExceptionCatch could be delivered. Delivery below
    // still re-resolves the exact VM and its enable set.
    if !crate::runtime::jvmti::any_listener_active() {
        return;
    }
    let thread_id = thread.thread_id.0;
    // Wave 12: the exact enable before the method id, which costs a
    // class-manager read and a method scan; the union above is raised by
    // any agent at all.
    if !crate::runtime::jvmti::exception_catch_enabled_for_vm(vm, thread_id) {
        return;
    }
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let method_id = jvmti_method_id(vm, frame);
    // Interpreter round i1 wave 52, lane L1: the caught object, which every
    // caller has just pushed as the handler frame's only operand (the frame
    // roots it while the callbacks run); `0` if a caller ever has not.
    let exception = match frame.stack.peek_checked() {
        // Cast: the object's heap address, as the other object events carry it
        Ok(Value::Object(Some(exc))) => exc.as_ptr() as usize as u64,
        _ => 0,
    };
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_exception_catch_for_vm(
            vm,
            thread_id,
            method_id,
            // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
            handler_pc as i64,
            exception,
        )
    });
}

/// The JVMTI `MethodId` an event about `frame` carries.
///
/// The real `(class_id << 32) | method_index` id — the encoding
/// `VmClassMethodProvider` (`vm/src/jvmti/mod.rs`) and JNI's
/// `encode_method_id` hand out — so an agent can pass it back to
/// `GetMethodName` or use it as a `jmethodID`. Every event used to carry
/// [`synth_method_id`]'s hash instead, which JNI decoded to an arbitrary
/// method of the class (or none), and which gave every overload of a name the
/// same id.
///
/// Falls back to [`synth_method_id`] when the VM cannot be reached (a bare
/// unit-test `SharedVm` with no registered bridge) or the class does not
/// declare the frame's method. Only called with a listener active: the lookup
/// is a registry read, a class-manager read and a scan of one method table.
#[inline]
pub(crate) fn jvmti_method_id(vm: usize, frame: &Frame) -> u64 {
    crate::runtime::jvmti::resolve_method_id_for_vm(
        vm,
        frame.class_id.as_u32(),
        frame.method_name(),
        frame.method_descriptor(),
    )
    .unwrap_or_else(|| synth_method_id(frame))
}

/// A synthesized JVMTI `MethodId` for `frame`, used only when
/// [`jvmti_method_id`] cannot resolve the real one.
///
/// Packs the 32-bit class id in the upper 32 bits and an FNV-1a hash of the
/// method name AND descriptor in the lower 32 (the descriptor used to be left
/// out, so every overload of a name shared one id). It is not a `jmethodID`.
#[inline]
pub(crate) fn synth_method_id(frame: &Frame) -> u64 {
    synth_method_id_of(frame.class_id, frame.method_name(), frame.method_descriptor())
}

/// [`jvmti_method_id`] for a method named without a frame: one running
/// compiled (interpreter round i4 wave 92, lane L1; the compiled `athrow`'s
/// site and a compiled catch, `interpreter::post_jvmti_exception`).
pub(crate) fn jvmti_method_id_of(
    vm: usize,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> u64 {
    crate::runtime::jvmti::resolve_method_id_for_vm(
        vm,
        class_id.as_u32(),
        method_name,
        descriptor,
    )
    .unwrap_or_else(|| synth_method_id_of(class_id, method_name, descriptor))
}

/// [`synth_method_id`] from its parts.
fn synth_method_id_of(class_id: ClassId, method_name: &str, descriptor: &str) -> u64 {
    let class_id = class_id.as_u32();
    let mut h: u32 = 2166136261;
    let name = method_name.bytes();
    // `(` never occurs in a method name, and every descriptor starts with it,
    // so the concatenation is unambiguous without a separator.
    for b in name.chain(descriptor.bytes()) {
        // Widening: smaller value -> u32 (value fits)
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
    ((class_id as u64) << 32) | (h as u64)
}

// ---------------------------------------------------------------------------
// T17.Δ — interpreter-side JVMTI event fire helpers
// ---------------------------------------------------------------------------
//
// These helpers wrap the per-event fast-path check + free-function fire so
// the interpreter hot path stays compact. Every helper returns early on a
// single `AtomicBool::Acquire` load when no agent is subscribed to the
// corresponding event, adding < 2 ns per opcode in the no-agent case.
//
// **VM scoping.** Each helper takes `vm: usize` — the raising VM's
// `SharedVm::vm_identity` — as its first parameter, and delivers through the
// `runtime::jvmti::fire_*_for_vm` family, which resolves that VM's JVMTI
// environment and re-checks *that environment's* enable set before invoking a
// callback. The `any_*_listener_active()` calls immediately below are the
// deliberate process-wide **union** pre-filter: with two VMs the union can be
// true because the *other* VM has an agent, which costs this VM one predicted
// branch plus one registry lookup and never delivers it another VM's event.
// Guards may over-approximate; delivery may not.
//
// `vm` is a parameter and not a field read off `thread`/`frame` because
// neither `JvmThread` (`vm/src/threading/jvm_thread.rs:336`) nor `Frame`
// (`vm/src/runtime/frame.rs:249`) carries a VM identity, and the only
// process-level `SharedVm` registry (`set_global_shared_vm_for_hooks`,
// `vm/src/vm/vm_init.rs:3498`) is a fan-out list of *every* live VM — it can
// answer "which VMs exist", never "which VM is running this frame". Guessing
// there would be exactly the wrong-VM delivery bug this scoping exists to
// close. Every caller of every helper below has `shared: &SharedVm` in scope,
// so the identity is always exact.

/// Fire `MethodEntry` for the frame at `frames_depth - 1` (the one just
/// pushed). Costs a single Acquire load when no agent is attached.
///
/// Currently unused — `push_frame_and_fire_entry` inlines the equivalent
/// logic so that the frame push and the event are a single chokepoint.
/// Retained for callers that already hold the pushed frame.
#[allow(dead_code)]
#[inline]
pub(super) fn fire_jvmti_method_entry(vm: usize, thread: &mut JvmThread, frame_idx: usize) {
    if !crate::runtime::jvmti::any_method_entry_listener_active() {
        return;
    }
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let method_id = jvmti_method_id(vm, frame);
    let tid = thread.thread_id.0;
    // gci i11/ja: delivered under this thread's own handle.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
    });
}

/// Fire `MethodExit` for a normal return from frame `frame_idx` with the
/// given return value.
///
/// gci i11/ja (item 1b of
/// `gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`):
/// takes the thread's `&mut` and the frame's index (it took `&JvmThread` and
/// a `&Frame` borrowed from it), so the delivery runs under
/// [`post_on_own_thread`].
#[inline]
pub(super) fn fire_jvmti_method_exit_normal(
    vm: usize,
    thread: &mut JvmThread,
    frame_idx: usize,
    return_value: &Option<Value>,
) {
    if !crate::runtime::jvmti::any_method_exit_listener_active() {
        return;
    }
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let method_id = jvmti_method_id(vm, frame);
    let lv = to_local_value(return_value.as_ref());
    let tid = thread.thread_id.0;
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_method_exit_for_vm(vm, tid, method_id, false, lv)
    });
}

/// Fire `MethodExit` for an exception-unwind exit.  The return value is
/// always `LocalValue::Object(None)` because the method did not produce a
/// value.
///
/// Currently unused — `pop_and_recycle_frame_with_reason` inlines the
/// equivalent logic so the event fires exactly once per unwind.  Retained
/// so external callers (e.g. a future JIT deopt path) can invoke it
/// directly without duplicating the fast-path gate.
#[allow(dead_code)]
#[inline]
pub(super) fn fire_jvmti_method_exit_exception(vm: usize, thread: &mut JvmThread, frame_idx: usize) {
    if !crate::runtime::jvmti::any_method_exit_listener_active() {
        return;
    }
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let method_id = jvmti_method_id(vm, frame);
    let tid = thread.thread_id.0;
    // gci i11/ja: delivered under this thread's own handle.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_method_exit_for_vm(
            vm,
            tid,
            method_id,
            /*was_popped_by_exception=*/ true,
            crate::runtime::jvmti::LocalValue::Object(None),
        )
    });
}

/// Fire `FramePop` if the about-to-be-popped frame's depth matches any
/// entry in `thread.frame_pop_requests`.  The matching entry is consumed
/// so that a single `NotifyFramePop` call yields exactly one event.
///
/// The request is consumed even when no `FramePop` listener is active at the
/// pop (interpreter round i1 wave 10, lane L4). It belongs to the frame, which
/// is leaving: kept, it matched the NEXT frame to pop at the same depth once
/// the event was enabled again and reported that unrelated frame. The request
/// list is checked first because it is empty on every pop without an agent
/// request, and it is this thread's own memory.
#[inline]
pub(super) fn fire_jvmti_frame_pop_if_requested(
    vm: usize,
    thread: &mut JvmThread,
    was_popped_by_exception: bool,
) {
    if thread.frame_pop_requests.is_empty() {
        return;
    }
    // Widening: smaller value -> u32 (value fits)
    let current_depth = thread.frames.len().saturating_sub(1) as u32;
    let Some(pos) = thread
        .frame_pop_requests
        .iter()
        .position(|d| *d == current_depth)
    else {
        return;
    };
    thread.frame_pop_requests.swap_remove(pos);
    if !crate::runtime::jvmti::any_frame_pop_listener_active() {
        return;
    }
    let Some(frame) = thread.frames.last() else {
        return;
    };
    let method_id = jvmti_method_id(vm, frame);
    let tid = thread.thread_id.0;
    // gci i11/ja: the agent's callback reborrows this frame's `&mut`.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_frame_pop_for_vm(vm, tid, method_id, was_popped_by_exception)
    });
}

/// Check single-step for this thread once per dispatched instruction.
/// Cost when no agent is subscribed: a single `AtomicBool::Acquire` load
/// (the per-event flag) plus one predicted branch.  No work otherwise.
///
/// gci i11/ja (item 1b): the thread's `&mut` and the stepped frame's index
/// (it took `&JvmThread` and a `&Frame` borrowed from it); the delivery runs
/// under [`post_on_own_thread`].
#[inline]
pub(super) fn fire_jvmti_single_step(
    vm: usize,
    thread: &mut JvmThread,
    frame_idx: usize,
    saved_pc: usize,
) {
    if !crate::runtime::jvmti::any_single_step_listener_active() {
        return;
    }
    // Per VM, then per thread in the delivery: the VM's manager and each env
    // attached to it deliver only to a thread they enabled the event for
    // (`JvmtiEventManager::deliver`, `own_event_enabled(kind, thread)`), which
    // is exactly JVMTI's per-thread `SetEventNotificationMode`. Interpreter
    // round i1 wave 22, lane L1: the gate here was
    // `JvmThread::single_step_enabled`, which nothing outside tests ever
    // raised, so no agent ever received a `SingleStep` (enabled globally or
    // for its thread).
    if !crate::runtime::jvmti::any_single_step_listener_active_for_vm(vm) {
        return;
    }
    let Some(frame) = thread.frames.get(frame_idx) else {
        return;
    };
    let method_id = jvmti_method_id(vm, frame);
    let tid = thread.thread_id.0;
    post_on_own_thread(thread, || {
        // Widening: smaller integer -> 64-bit (zero/sign-extended, value preserved)
        crate::runtime::jvmti::fire_single_step_for_vm(vm, tid, method_id, saved_pc as i64)
    });
}

/// Fire `MethodEntry` for the frame on top of the stack.
///
/// Split out of [`push_frame_and_fire_entry`] for the fast doors, which
/// install a callee by rebuilding a retired `FrameStack` slot in place and so
/// never hand a `Frame` to that function. Only the JVMTI event is shared: the
/// Spring trace and the bytecode dump beside it are diagnostics of the
/// by-value push and stay there.
///
/// Interpreter round i1 wave 24: also where a JDWP `MethodEntry` request
/// learns of the push. While one is in force in any VM of the process (one
/// relaxed load, `debug::method_entry_requests_anywhere`), the new frame's
/// depth is recorded for the suspend point, which reports the entry at the
/// frame's first bytecode (`debug::take_frame_entry`), where it can suspend
/// the thread; a backward branch to bytecode 0 is then not an entry.
#[inline]
pub(crate) fn fire_method_entry_after_push(vm: usize, thread: &mut JvmThread) {
    fire_method_entry_after_push_with(vm, None, thread);
}

/// [`fire_method_entry_after_push`] for a frame an interpreter invoke pushed
/// for the dispatch loop that runs its caller (the fast doors, the general
/// dispatchers' [`install_cached_frame`], the stackless invoke): that loop
/// runs the new frame next, at `frame_idx + 1`.
///
/// Interpreter round i4 wave 95, lane L7 (stage 1 of
/// `docs/internal/proposals/interpreter/i62-L1-proposal-one-agent-request-word-the-dispatch-loop-reads-20261026.md`;
/// `VmConfig::jvmti_method_entry_frame_control`,
/// `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`, default ON since wave 129): with the switch
/// on, a C agent's `MethodEntry` callback runs at a callback point
/// ([`fire_method_entry_at_callback_point`]), so its `PopFrame` or
/// `ForceEarlyReturn*` of the new frame is recorded and served before the
/// frame's bytecode 0, as HotSpot serves it. Without a `MethodEntry` listener
/// this is [`fire_method_entry_after_push`] exactly (the switch is read only
/// behind the listener load that function already makes).
#[inline]
pub(crate) fn fire_method_entry_after_push_in_loop(shared: &SharedVm, thread: &mut JvmThread) {
    fire_method_entry_after_push_with(shared.vm_identity, Some(shared), thread);
}

/// The body of both entry hooks above; `in_loop` is `Some` for a push the
/// caller's own dispatch loop runs next.
#[inline(always)]
fn fire_method_entry_after_push_with(
    vm: usize,
    in_loop: Option<&SharedVm>,
    thread: &mut JvmThread,
) {
    // The i54-L6 retirement: a synchronized method whose contended monitor
    // is entered after this push posts its `MethodEntry` then
    // ([`fire_deferred_method_entry`]), as HotSpot posts it after
    // `lock_method`.
    if crate::runtime::jvmti::any_method_entry_listener_active()
        && thread.deferred_method_entry_depth != thread.frames.len()
    {
        match in_loop {
            Some(shared) if shared.config.jvmti_method_entry_frame_control => {
                fire_method_entry_at_callback_point(shared, thread);
            }
            _ => {
                let last = thread.frames.len() - 1;
                let frame_ref = &thread.frames[last];
                let method_id = jvmti_method_id(vm, frame_ref);
                let tid = thread.thread_id.0;
                // gci i11/ja: the agent's callback reborrows this frame's `&mut`.
                post_on_own_thread(thread, || {
                    crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
                });
            }
        }
    }
    #[cfg(feature = "experimental-debug")]
    {
        if crate::debug::method_entry_requests_anywhere() {
            crate::debug::note_frame_entered(thread.frames.len());
        }
    }
}

/// Fire `MethodEntry` for the frame on top, which the dispatch loop running
/// its caller (frame `depth - 2`) runs next, at a callback point
/// ([`enter_callback_point`]; interpreter round i4 wave 95, lane L7,
/// `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`). The point's loop base is the
/// caller's index: the caller is a frame of that loop, so a `PopFrame` of the
/// new frame is admitted (`debug::pop_frames::request_jvmti_pop_in_callback`)
/// and its invoke runs again, as on HotSpot. A request recorded in the
/// callback raises `AGENT_REQUEST_FRAME_CONTROL` in the thread's agent-request
/// word and bumps this OS thread's loop poll word, so the loop's next top --
/// the new frame's bytecode 0 -- goes to its slow path, which takes the
/// request (`take_method_entry_frame_control`, the dispatch loop's reader).
/// Cold: only with a `MethodEntry` listener and the switch on.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
fn fire_method_entry_at_callback_point(shared: &SharedVm, thread: &mut JvmThread) {
    let vm = shared.vm_identity;
    let depth = thread.frames.len();
    let method_id = jvmti_method_id(vm, &thread.frames[depth - 1]);
    let tid = thread.thread_id.0;
    let point = enter_callback_point(shared, thread, depth.saturating_sub(2));
    // gci i11/ja: the agent's callback reborrows this frame's `&mut`.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
    });
    if point && leave_callback_point(shared, thread) {
        thread.gc_block_state.agent_requests.fetch_or(
            crate::threading::jvm_thread::AGENT_REQUEST_FRAME_CONTROL,
            std::sync::atomic::Ordering::Relaxed,
        );
        crate::threading::gc_barrier::note_code_moved_on_this_thread();
    }
}

/// [`fire_method_entry_at_callback_point`] in a build without the debugger
/// surface: no frame control from a callback, the event alone.
#[cfg(not(feature = "experimental-debug"))]
#[cold]
#[inline(never)]
fn fire_method_entry_at_callback_point(shared: &SharedVm, thread: &mut JvmThread) {
    let vm = shared.vm_identity;
    let last = thread.frames.len() - 1;
    let method_id = jvmti_method_id(vm, &thread.frames[last]);
    let tid = thread.thread_id.0;
    // gci i11/ja: the agent's callback reborrows this frame's `&mut`.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
    });
}

/// The dispatch loop's reader of [`fire_method_entry_at_callback_point`]'s
/// request (interpreter round i4 wave 95, lane L7), run only in the loop
/// top's slow path with `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` on:
/// clears `AGENT_REQUEST_FRAME_CONTROL` and answers whether the loop should
/// take the recorded request at frame `frame_idx` now -- the frame on top,
/// which is the one the callback was about (the record is keyed by the frame
/// count, so `take_callback_frame_control` takes nothing for any other). The
/// loop then forces its suspend point for this pc, as after a `SingleStep`
/// callback's request (wave 62's `step_frame_control`).
#[cold]
#[inline(never)]
pub(super) fn take_method_entry_frame_control(thread: &JvmThread, frame_idx: usize) -> bool {
    use crate::threading::jvm_thread::AGENT_REQUEST_FRAME_CONTROL;
    let word = &thread.gc_block_state.agent_requests;
    if word.load(std::sync::atomic::Ordering::Relaxed) & AGENT_REQUEST_FRAME_CONTROL == 0 {
        return false;
    }
    word.fetch_and(
        !AGENT_REQUEST_FRAME_CONTROL,
        std::sync::atomic::Ordering::Relaxed,
    );
    frame_idx + 1 == thread.frames.len()
}

/// The JVMTI `MethodEntry` [`fire_method_entry_after_push`] left out for the
/// top frame, a synchronized method pushed before its contended monitor
/// (`JvmThread::deferred_method_entry_depth`), now that the monitor is held
/// (the i54-L6 retirement). Clears the record whatever it names. Cold: only
/// a contended synchronized entry while a C agent listens for `MethodEntry`
/// gets here with one.
#[cold]
#[inline(never)]
pub(crate) fn fire_deferred_method_entry(vm: usize, thread: &mut JvmThread) {
    let depth = std::mem::replace(&mut thread.deferred_method_entry_depth, 0);
    if depth == 0
        || depth != thread.frames.len()
        || !crate::runtime::jvmti::any_method_entry_listener_active()
    {
        return;
    }
    let method_id = jvmti_method_id(vm, &thread.frames[depth - 1]);
    let tid = thread.thread_id.0;
    // gci i11/ja: the agent's callback reborrows this frame's `&mut`.
    post_on_own_thread(thread, || {
        crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, method_id)
    });
}

/// [`fire_deferred_method_entry`] for a frame an interpreter invoke pushed
/// for the dispatch loop that runs its caller, which runs the frame next
/// (interpreter round i6 wave 134, lane L1; shape 1 of
/// `docs/known-issues/interpreter/i133-L8-c-jvmti-method-entry-frame-control-residuals-20261231.md`).
/// With `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` and
/// `CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL` on, the callback runs at
/// the callback point the uncontended entry's runs at
/// ([`fire_method_entry_at_callback_point`]), so a `PopFrame` or a
/// `ForceEarlyReturn*` it makes of the new frame is taken before its
/// bytecode 0, as for the uncontended entry; the frame already owns the
/// monitor, which the pop or the forced return releases with it. Off, this
/// is [`fire_deferred_method_entry`] exactly.
#[cold]
#[inline(never)]
pub(crate) fn fire_deferred_method_entry_in_loop(shared: &SharedVm, thread: &mut JvmThread) {
    let config = &shared.config;
    if !(config.jvmti_method_entry_frame_control && config.jvmti_contended_entry_frame_control) {
        fire_deferred_method_entry(shared.vm_identity, thread);
        return;
    }
    let depth = std::mem::replace(&mut thread.deferred_method_entry_depth, 0);
    if !deferred_entry_names_top(depth, thread.frames.len())
        || !crate::runtime::jvmti::any_method_entry_listener_active()
    {
        return;
    }
    fire_method_entry_at_callback_point(shared, thread);
}

/// Does a deferred `MethodEntry` record (`JvmThread::deferred_method_entry_depth`,
/// 0 for none) name the frame on top of a stack of `frames` frames? The
/// test [`fire_deferred_method_entry`] and its in-loop twin make before they
/// post.
#[inline]
fn deferred_entry_names_top(depth: usize, frames: usize) -> bool {
    depth != 0 && depth == frames
}

/// gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 1, part 2): run `post` -- a `fire_*_for_vm` delivery to C agents --
/// with this thread's JNI binding (`JNI_THREAD`) installed from the poster's
/// own `&mut`, and the previous binding put back afterwards (also on unwind).
///
/// The agent callback's context (`jvmti::native_env::in_event_context`) then
/// keeps that pointer instead of installing the registry's address of the
/// same `JvmThread`. A GC-safe JVMTI function the callback calls
/// (`RawMonitorWait`, a self-`SuspendThread`, `ForceGarbageCollection`, the
/// heap walks) reborrows the installed pointer and writes through it (TLAB
/// retire, root deposit, frame fix-up). Derived from the registry it was a
/// second `&mut` of a thread whose `&mut` this poster holds (Rust aliasing
/// UB); derived from the poster's handle it is a reborrow of it, the shape
/// the native funnel and `run_agent_thread_body` already use. `thread` is
/// handed over for the call, so `post` cannot touch it.
///
/// gci i12/ev (item 1c): `post` may also be a thread-less VM operation whose
/// C deliveries happen somewhere inside it with no handle of this thread --
/// the class load of `SharedVm::load_class_transformed`, whose `ClassLoad`
/// the class manager's guard drop posts. The same rule holds: nothing in
/// `post` may reach this `JvmThread` except through the installed binding.
#[inline]
pub(crate) fn post_on_own_thread<R>(thread: &mut JvmThread, post: impl FnOnce() -> R) -> R {
    // SAFETY: derived from the caller's live `&mut`, which this call holds
    // (unused) until `post` returns.
    unsafe { post_on_own_thread_ptr(thread as *mut JvmThread, post) }
}

/// [`post_on_own_thread`] for a poster that already derived a raw pointer
/// from its `&mut` for something else that stays live across the post (a
/// `NativeFrameRow`); one provenance for both.
///
/// # Safety
///
/// `own` points at the executing thread's live `JvmThread`, derived from a
/// `&mut` the caller does not use again until this returns.
pub(crate) unsafe fn post_on_own_thread_ptr<R>(own: *mut JvmThread, post: impl FnOnce() -> R) -> R {
    struct Restore(*mut JvmThread);
    impl Drop for Restore {
        fn drop(&mut self) {
            // SAFETY: the pointer this OS thread had installed before the post.
            unsafe { crate::native::jni::restore_jni_thread(self.0) };
        }
    }
    let _restore = Restore(crate::native::jni::replace_jni_thread(own));
    post()
}

/// Push `frame` onto the thread and fire `MethodEntry`.  The MethodEntry
/// fire is gated on `any_method_entry_listener_active` — a single
/// `AtomicBool::Acquire` load when no agent is subscribed.
///
/// This is the chokepoint for every by-value interpreter frame push; the
/// in-place installs (`install_cached_frame`, the fast doors) reach the same
/// event through [`fire_method_entry_after_push`]. A push site that calls
/// `thread.frames.push` directly does NOT fire MethodEntry.
///
/// `vm` is `shared.vm_identity` at every call site. It cannot be derived from
/// `thread` or `frame` — see the VM-scoping note at the top of this helper
/// block.
pub(crate) fn push_frame_and_fire_entry(vm: usize, thread: &mut JvmThread, frame: Frame) {
    // A retired slot at this depth is about to be overwritten by the frame
    // just built. Harvest its buffers into the thread pools first, so the
    // pooled constructors keep the recycling they have always relied on —
    // without this, a workload whose calls do not go through a fast door
    // would free a set of buffers and allocate a fresh one every call.
    thread.harvest_retired_slot();
    thread.frames.push(frame);
    fire_entry_and_diagnostics_after_push(vm, thread);
}

/// [`push_frame_and_fire_entry`] for a frame the caller's own dispatch loop
/// runs next ([`fire_method_entry_after_push_in_loop`]; interpreter round i4
/// wave 95, lane L7).
pub(crate) fn push_frame_and_fire_entry_in_loop(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
) {
    thread.harvest_retired_slot();
    thread.frames.push(frame);
    fire_method_entry_after_push_in_loop(shared, thread);
    push_diagnostics_after_push(thread);
}

/// Push a frame REBUILT from a compiled activation — a deopt resume, or a
/// compiled method's exception routed into its own handler — WITHOUT
/// `MethodEntry`, and otherwise exactly as [`push_frame_and_fire_entry`].
///
/// The method was entered in compiled code, so its entry happened then, not
/// at the resume bci. HotSpot posts no `MethodEntry` for a deoptimized frame
/// either; the frame's `MethodExit` is still posted when it returns through
/// the interpreter, as it is for an interpreted frame that was already running
/// when the agent enabled the event. Firing it here reported the method as
/// entered mid-body, at the resume pc (interpreter round i1 wave 9; see
/// `docs/internal/fixed-bugs/interpreter-L7-jvmti-events-lost-in-compiled-code-FIXED-20260925.md`).
pub(crate) fn push_resumed_frame(thread: &mut JvmThread, frame: Frame) {
    thread.harvest_retired_slot();
    thread.frames.push(frame);
    // Interpreter round i4 wave 93 (lane L1): a JDWP method-entry record left
    // at this depth by an earlier frame the suspend point never reached (an
    // unconcerned one, `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`) is not this
    // frame's: a backward branch of the resumed method to bytecode 0 is no
    // entry. One relaxed load with no `MethodEntry` request.
    #[cfg(feature = "experimental-debug")]
    {
        if crate::debug::method_entry_requests_anywhere() {
            crate::debug::note_frame_resumed(thread.frames.len());
        }
    }
    push_diagnostics_after_push(thread);
}

/// Install a frame for a cached bytecode callee on top of `thread`'s stack.
///
/// The general dispatchers' counterpart to the fast doors'
/// `invoke_fast::push_frame_verbatim`, and the same three-way ladder:
///
/// 1. **Rebuild the retired slot** at this depth. A return retires its frame
///    in place, so the four buffers are already here and none of them has to
///    travel through the thread's pools.
/// 2. **Emplace** into the next slot when none is retired — the first call at
///    a depth. Only the three buffer handles travel; the ~220-byte `Frame`
///    is written where it belongs instead of being built and moved.
/// 3. **By value**, which is what every general dispatcher did before this and
///    what `CRATONVM_JIT_NO_FRAME_EMPLACE` restores.
///
/// The one thing the doors do that this cannot is the verbatim `(slot, tag)`
/// argument transfer: a general dispatcher has already decoded its arguments
/// to `Value`s by the time it knows which callee shape it has.
///
/// `monitor_obj` is installed *after* the frame is, because two of the three
/// paths never hold a `Frame` to set it on. That is not a reordering of the
/// monitor **enter** — every caller still does that before calling this, and
/// must, since a synchronized callee's monitor has to be held before its frame
/// can run. `trace_tag` reports the caller's depth, as it did when the trace
/// ran before the push.
///
/// For a frame the caller's own dispatch loop runs next (every general
/// dispatcher answering `CachedCallResult::FramePushed`): its `MethodEntry`
/// goes through [`fire_method_entry_after_push_in_loop`]. A caller that runs
/// the frame in a loop of its own (`install_and_run_cached_frame*`) uses
/// [`install_cached_frame_nested`] (interpreter round i4 wave 95, lane L7).
#[inline(always)]
pub(crate) fn install_cached_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    monitor_obj: Option<ObjectRef>,
    trace_tag: Option<&str>,
    charge_phases: bool,
) {
    install_cached_frame_for::<true>(
        shared,
        thread,
        cached,
        args,
        monitor_obj,
        trace_tag,
        charge_phases,
    );
}

/// [`install_cached_frame`] for a caller that runs the frame in a dispatch
/// loop of its own, whose entry frame it is (`install_and_run_cached_frame*`):
/// its `MethodEntry` goes through [`fire_method_entry_after_push`], as before
/// wave 95 (no frame control from the callback: the frame's caller is not a
/// frame of that loop).
#[inline(always)]
pub(crate) fn install_cached_frame_nested(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    monitor_obj: Option<ObjectRef>,
    trace_tag: Option<&str>,
    charge_phases: bool,
) {
    install_cached_frame_for::<false>(
        shared,
        thread,
        cached,
        args,
        monitor_obj,
        trace_tag,
        charge_phases,
    );
}

/// The body of [`install_cached_frame`] (`IN_LOOP`) and
/// [`install_cached_frame_nested`].
#[allow(clippy::too_many_arguments)]
fn install_cached_frame_for<const IN_LOOP: bool>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    monitor_obj: Option<ObjectRef>,
    trace_tag: Option<&str>,
    charge_phases: bool,
) {
    use crate::runtime::interpreter::invoke_phases;
    // Only `execute_invokestatic_cached` charges the phase accounting, and
    // `now()` is a relaxed load even when the instrument is off. The other
    // four call sites should not pay for an instrument they do not feed.
    let ph_t0 = if charge_phases {
        invoke_phases::now()
    } else {
        0
    };
    let depth_before = thread.frames.len();

    if !shared.config.frame_emplace {
        // The control arm: build the frame somewhere else and move it in.
        let mut frame = Frame::new_pooled_cached(
            cached,
            args,
            &mut thread.locals_pool,
            &mut thread.stacks_pool,
        );
        frame.monitor_on_exit = monitor_obj;
        trace_frame_push(trace_tag, depth_before, &frame);
        let ph_t1 = if charge_phases {
            invoke_phases::now()
        } else {
            0
        };
        // `push_frame_and_fire_entry` harvests the retired slot, so this arm
        // also turns slot reuse off for the general dispatchers — which is
        // exactly the state they were in before this change.
        if IN_LOOP {
            push_frame_and_fire_entry_in_loop(shared, thread, frame);
        } else {
            push_frame_and_fire_entry(shared.vm_identity, thread, frame);
        }
        super::gci_i11jd_witness::note_entry(shared, thread);
        site_stats::bump(site_stats::INSTALL_BYVALUE);
        charge_install(charge_phases, ph_t0, ph_t1);
        return;
    }

    // `has_retired_slot` is exactly when `push_cached_value_reusing` succeeds,
    // so `cached` is moved into the slot rather than cloned and dropped (two
    // atomic refcount updates per reusing push; interpreter round i1 wave 22,
    // lane L4, the same change as `invoke_fast::push_frame_verbatim`).
    if shared.config.frame_slot_reuse && thread.frames.has_retired_slot() {
        // A slot still holding pooled buffers is converted to a slab window
        // once (wave 32), as in `invoke_fast::push_frame_verbatim`.
        if thread.frames.retired_slot_holds_pooled_buffers() {
            thread.convert_retired_slot_to_window();
        }
        let pushed = thread.frames.push_cached_value_reusing(cached, args);
        debug_assert!(pushed, "a retired slot is always reusable");
        let ph_t1 = if charge_phases {
            invoke_phases::now()
        } else {
            0
        };
        install_tail::<IN_LOOP>(shared, thread, monitor_obj, trace_tag, depth_before);
        site_stats::bump_if(shared.config.site_census, site_stats::INSTALL_REUSE);
        charge_install(charge_phases, ph_t0, ph_t1);
        return;
    }

    // No slot to rebuild: harvest anything retired above us so its buffers
    // reach the pools the parts build is about to draw from, then write the
    // frame straight into the slot.
    thread.harvest_retired_slot();
    // In one window of the frame stack's slot slab, or (under
    // `CRATONVM_JIT_NO_LOCALS_SLAB`) the pooled parts emplaced as before.
    thread.frames.emplace_cached_value_args(
        cached,
        args,
        &mut thread.locals_pool,
        &mut thread.stacks_pool,
    );
    let ph_t1 = if charge_phases {
        invoke_phases::now()
    } else {
        0
    };
    install_tail::<IN_LOOP>(shared, thread, monitor_obj, trace_tag, depth_before);
    site_stats::bump(site_stats::INSTALL_EMPLACE);
    charge_install(charge_phases, ph_t0, ph_t1);
}

/// `P_FRAME_BUILD` for the install, `P_PUSH` for the tail after it.
#[inline]
fn charge_install(charge_phases: bool, start: u64, tail_start: u64) {
    if !charge_phases {
        return;
    }
    use crate::runtime::interpreter::invoke_phases;
    invoke_phases::charge(invoke_phases::P_FRAME_BUILD, start, tail_start);
    invoke_phases::charge(invoke_phases::P_PUSH, tail_start, invoke_phases::now());
}

/// What both in-place install paths do once the frame is live: the monitor the
/// caller already entered, the frame trace, the JVMTI entry event and the
/// push-time diagnostics. `IN_LOOP`: the caller's own dispatch loop runs the
/// frame next ([`fire_method_entry_after_push_in_loop`], wave 95).
#[inline]
fn install_tail<const IN_LOOP: bool>(
    shared: &SharedVm,
    thread: &mut JvmThread,
    monitor_obj: Option<ObjectRef>,
    trace_tag: Option<&str>,
    depth_before: usize,
) {
    if monitor_obj.is_some() {
        if let Some(top) = thread.frames.last_mut() {
            top.monitor_on_exit = monitor_obj;
        }
    }
    if trace_tag.is_some() {
        let top = thread.frames.len() - 1;
        let frame = &thread.frames[top];
        trace_frame_push(trace_tag, depth_before, frame);
    }
    if IN_LOOP {
        fire_method_entry_after_push_in_loop(shared, thread);
        push_diagnostics_after_push(thread);
    } else {
        fire_entry_and_diagnostics_after_push(shared.vm_identity, thread);
    }
    super::gci_i11jd_witness::note_entry(shared, thread);
}

/// `CRATONVM_FRAME_TRACE=1` — one line per frame push, at the depth the
/// caller was at.
#[inline]
fn trace_frame_push(tag: Option<&str>, depth: usize, frame: &Frame) {
    let Some(tag) = tag else { return };
    if !crate::runtime::env_cache::frame_trace() {
        return;
    }
    eprintln!(
        "[FRAME_PUSH/{}] depth={} {}.{}{}",
        tag,
        depth,
        frame.class_name(),
        frame.method_name(),
        frame.method_descriptor()
    );
}

/// The JVMTI `MethodEntry` event plus every push-time diagnostic that reads
/// the frame just installed.
///
/// Split out of [`push_frame_and_fire_entry`] so that
/// [`install_cached_frame`]'s in-place paths, which never hand a `Frame` to
/// that function, still run all of them — an `SBF-TRACE` that goes dark for
/// the calls that happen to take a cheaper install is worse than no trace.
pub(crate) fn fire_entry_and_diagnostics_after_push(vm: usize, thread: &mut JvmThread) {
    fire_method_entry_after_push(vm, thread);
    push_diagnostics_after_push(thread);
}

/// The push-time diagnostics of [`fire_entry_and_diagnostics_after_push`]
/// without the JVMTI event, for [`push_resumed_frame`].
pub(crate) fn push_diagnostics_after_push(thread: &JvmThread) {
    if crate::runtime::env_cache::trace_sb_filter() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        let cn = frame_ref.class_name();
        let mn = frame_ref.method_name();
        if cn.contains("FilteringSpringBootCondition")
            || cn.contains("OnClassCondition")
            || cn.contains("OnBeanCondition")
            || cn.contains("OnWebApplicationCondition")
            || cn.contains("AutoConfigurationImportSelector")
            || cn.contains("AutoConfigurationImportFilter")
            || (cn.contains("SpringFactoriesLoader")
                && (mn == "loadFactories" || mn == "loadFactoryNames" || mn == "load"))
            || cn.contains("ImportCandidates")
        {
            eprintln!(
                "[SBF-TRACE] enter {}.{}{}",
                cn,
                mn,
                frame_ref.method_descriptor()
            );
        }
    }
    // TEMP DIAGNOSTIC (CRATONVM_DBG_BYTECODE_DUMP, 2026-07-15
    // JRubyScriptTemplateTests investigation, round 3): dump the raw
    // bytecode + a best-effort mnemonic disassembly for every frame whose
    // class name matches the runtime-generated JRuby snippet under
    // investigation (`uri_3a_classloader....rubygems.version`, the
    // URI-mangled class name JRuby's IR-to-bytecode compiler produces for
    // `rubygems/version.rb`'s method bodies). These snippets are
    // synthesized at runtime -- there is no static .class file `javap` can
    // decompile -- so this is the only way to see the literal opcode
    // sequence CratonVM is actually executing around the `ivarGet`/
    // `invoke:sub` invokedynamic call sites.
    if crate::runtime::env_cache::dbg_bytecode_dump() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        let cn = frame_ref.class_name();
        if cn.contains("version") {
            let mn = frame_ref.method_name();
            let md = frame_ref.method_descriptor();
            let code = &frame_ref.code;
            eprintln!("[BYTECODE-DUMP] {}.{}{} ({} bytes)", cn, mn, md, code.len());
            let mut pc = 0usize;
            while pc < code.len() {
                let op = code[pc];
                let (mnemonic, extra_len) = opcode_mnemonic_and_operand_len(op, code, pc);
                let operand_bytes: Vec<u8> =
                    code[pc + 1..(pc + 1 + extra_len).min(code.len())].to_vec();
                eprintln!(
                    "  {:4}: {:02x} {:<20} operands={:?}",
                    pc, op, mnemonic, operand_bytes
                );
                pc += 1 + extra_len;
            }
        }
    }
    // TEMP DIAGNOSTIC (CRATONVM_DBG_DUPCALL_FILTER, 2026-07-23, WFLYCTL0079
    // investigation): "An attribute named 'hornetq-store-enable-async-io'
    // is already registered at location '/subsystem=transactions'" fires
    // ~1/1600 WildFly boots from inside
    // ParallelExtensionAddHandler$ExtensionInitializeTask.call(), which
    // decompiled bytecode shows invokes `ExtensionAddHandler.
    // initializeExtension(module, ...)` exactly once per task instance —
    // each extension gets exactly one task submitted to the boot executor.
    // A single-threaded, deterministic registration bug inside WildFly
    // would fail EVERY boot, not ~1/1600, so the leading hypothesis is
    // that the SAME task object's `call()` is somehow entered twice (an
    // executor/queue double-dispatch race) rather than a WildFly-side
    // logic bug. This is directly testable: log (receiver identity,
    // thread, entry ordinal) on every entry to this one method and see if
    // the same receiver address appears twice. Filtered by exact class
    // name (not a substring) since this must be cheap enough to run a
    // multi-hundred-boot campaign with it always on.
    if crate::runtime::env_cache::dbg_dupcall_filter() {
        let last = thread.frames.len() - 1;
        let frame_ref = &thread.frames[last];
        if frame_ref.method_name() == "call"
            && frame_ref.class_name()
                == "org/jboss/as/controller/extension/ParallelExtensionAddHandler$ExtensionInitializeTask"
        {
            let recv = frame_ref.get_local(0);
            let recv_addr = match recv {
                Value::Object(Some(o)) => o.as_ptr() as usize,
                _ => 0,
            };
            static ORDINAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let ord = ORDINAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "[DUPCALL] #{ord} ExtensionInitializeTask.call() recv=0x{recv_addr:x} tid={}",
                thread.thread_id.0,
            );
        }
        // WFLYCTL0079 round 2: the executor-dispatch trace above (each
        // ExtensionInitializeTask.call() runs exactly once, modulo the
        // Callable<V> bridge pair) never caught a genuine repeat in 1600
        // boots. The actual registry mutation is several frames deeper —
        // `ExtensionAddHandler.initializeExtension` eventually calls
        // `registerAttributes(ManagementResourceRegistration)` on the
        // transactions subsystem's root resource definition, which is
        // where `HORNETQ_STORE_ENABLE_ASYNC_IO` actually gets registered
        // (via an `AliasedHandler`, decompiled bytecode confirms). A
        // retry/re-registration at THIS level wouldn't require
        // `call()` itself to re-run. Trace (receiver, registration-arg)
        // identity pairs directly at the registration call site instead.
        if frame_ref.method_name() == "registerAttributes"
            && frame_ref.class_name()
                == "org/jboss/as/txn/subsystem/TransactionSubsystemRootResourceDefinition"
        {
            let recv = frame_ref.get_local(0);
            let recv_addr = match recv {
                Value::Object(Some(o)) => o.as_ptr() as usize,
                _ => 0,
            };
            let reg = frame_ref.get_local(1);
            let reg_addr = match reg {
                Value::Object(Some(o)) => o.as_ptr() as usize,
                _ => 0,
            };
            static ORDINAL2: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let ord = ORDINAL2.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "[DUPREG] #{ord} TransactionSubsystemRootResourceDefinition.registerAttributes() recv=0x{recv_addr:x} registration=0x{reg_addr:x} tid={}",
                thread.thread_id.0,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Leaving compiled loops when the interpreter is required
// (interpreter round i1 wave 12, lane L4)
// ---------------------------------------------------------------------------
//
// The doors stop ENTERING compiled code once an agent needs the interpreter
// (`jit_bridge::jvmti_requires_interpreter*`), but a loop already running in
// an OSR body used to run to completion with no event. It now leaves at its
// next back-edge poll that takes the slow path: `jit_safepoint_slow_path`
// returns [`compiled_loop_must_leave`] and an OSR body's `goto` back-edge
// poll branches on it to the loop header's OSR-exit map (a conditional or
// switch back edge, since wave 13, to a map at the branch's own bci, with its
// operands), which `try_osr` transfers into the live frame. [`request_compiled_loop_exits`] makes the
// polls take the slow path when the mode comes into force. What still
// finishes compiled is listed in
// `docs/internal/fixed-bugs/interpreter-L5-jvmti-frames-already-compiled-finish-compiled-FIXED-20261005.md`.

/// The verdict of a compiled back-edge poll's slow path: must the frame
/// `thread` is running run interpreted now?
///
/// The frame is the innermost interpreter frame, which is the OSR'd frame
/// whenever the verdict is read (only an OSR body's back-edge poll reads it,
/// and that body runs inside the frame's activation). The question is the
/// OSR door's own (`jvmti_requires_interpreter_for_method`): a JVMTI
/// interpreter-only event anywhere in the VM, or a JDWP request concerning
/// this method. Asking the same question means an exit is never followed by
/// an immediate re-entry the door would allow.
pub(crate) fn compiled_loop_must_leave(shared: &SharedVm, thread: &JvmThread) -> bool {
    let Some(frame) = thread.frames.last() else {
        return false;
    };
    super::jvmti_requires_interpreter_for_method(
        shared,
        frame.class_id,
        frame.method_name(),
        frame.method_descriptor(),
    )
}

// Interpreter round i1 wave 15, lane L3: METHOD-ENTRY bodies leave too.
//
// A method-entry body's back-edge (and self-tail) poll has no interpreter
// frame of its own, so [`compiled_loop_must_leave`]'s answer is about some
// other method. The slow path therefore returns a second bit,
// `SAFEPOINT_VERDICT_POLLING_BODY` (wave 15's `SAFEPOINT_VERDICT_EVERY_FRAME`),
// and a method-entry body tests that bit alone. Its exit reaches the
// de-speculating stash sinks, which take it uncharged
// ([`exit_left_for_the_interpreter`]).
//
// Wave 17, lane L1 (`i15-L3-proposal-poll-slow-path-names-its-compiled-body`):
// the single-pass poll names its body (the slow path's second argument, the
// artifact's compile id, confirmed against the thread's compile-id mirror), so
// the bit is about THAT body's method ([`polling_body_must_leave`]): a JDWP
// breakpoint in the method pulls its running body back, and a redefinition
// withholds the exits of the redefined class's bodies only. A body the poll
// does not name keeps the wave-15 answer, about every compiled frame
// ([`every_compiled_frame_may_leave`]).

/// The compiled body whose safepoint poll asked for a verdict: the artifact's
/// owner class and its `Class.method:descriptor` label
/// (`CompiledMethod::method_label`, the key both tiers stamp).
///
/// Wave 18, lane L3: also the address range of the artifact's
/// `deopt_points` (where a single-pass body's exit points live, so a sink can
/// tell this body's exit by the stashed point address) and its
/// `install_epoch` (whether it was compiled after the VM's last redefinition).
/// Both are unknown (`(0, 0)`, `None`) for a body built by
/// [`Self::from_artifact`] alone.
///
/// Wave 19, lane L2: and the artifact's boxed points (`_deopt_point_boxes`),
/// where an optimizing body's back-edge poll exits live, outside that range
/// (empty for a single-pass body, and when unknown).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PollingBody<'a> {
    class_id: ClassId,
    class_name: &'a str,
    method_name: &'a str,
    descriptor: &'a str,
    deopt_points: (usize, usize),
    exit_boxes: &'a [Box<cratonvm_jit::deopt::DeoptimizationPoint>],
    install_epoch: Option<u64>,
    /// Interpreter round i1 wave 23, lane L6: a class redefinition withdrew
    /// the artifact (`CompiledMethod::is_withdrawn_by_redefinition`), so it
    /// may run a redefined class's old bytecode -- a splice of a redefined
    /// callee. `false` when unknown.
    withdrawn_by_redefinition: bool,
    /// Interpreter round i1 wave 45, lane L2: an OSR body of its own class's
    /// old bytecode, forced to leave by a redefinition that renumbered the
    /// class's pool (`CompiledMethod::leaves_as_renumbered_obsolete`), whose
    /// frame the OSR door's in-place transfer resumes
    /// ([`withdrawn_body_may_leave`]). `false` when unknown.
    leaves_as_renumbered_obsolete: bool,
    /// Interpreter round i1 wave 46, lane L2: for a METHOD-ENTRY body of that
    /// kind, the bytecode it was compiled from and that bytecode's pool
    /// generation, which the grant keeps for the stash sink that resumes the
    /// exit ([`granted_obsolete_source`]). `None` otherwise.
    obsolete_source: Option<ObsoleteEntrySource<'a>>,
    /// Interpreter round i2 wave 53, lane L2: whether a compiled caller bound
    /// the artifact for a caller-held synchronized `CALL`
    /// (`CompiledMethod::is_bound_by_a_caller_held_site`). `None` when
    /// unknown, which [`compiled_frame_exit_verdict`] treats as bound: it
    /// sets `SAFEPOINT_VERDICT_NAMED_BODY` only for `Some(false)`.
    caller_held_binding: Option<bool>,
}

/// The bytecode a method-entry body forced to leave as its class's obsolete
/// activation was compiled from (`CompiledMethod::compiled_source`) and its
/// constant-pool generation (`CompiledMethod::compile_cp_stamp`); see
/// [`PollingBody::with_obsolete_source`]. Interpreter round i1 wave 46, lane
/// L2.
#[derive(Clone, Copy)]
pub(crate) struct ObsoleteEntrySource<'a> {
    pub(crate) source: &'a Arc<CachedBytecodeMethod>,
    pub(crate) stamp: u64,
}

impl std::fmt::Debug for ObsoleteEntrySource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}{}@{}",
            self.source.class_name,
            self.source.method_name,
            self.source.method_descriptor,
            self.stamp
        )
    }
}

/// [`ObsoleteEntrySource`] as a grant keeps it past the poll: the source
/// shared, compared by identity.
#[derive(Clone)]
struct GrantedObsoleteSource {
    source: Arc<CachedBytecodeMethod>,
    stamp: u64,
}

impl std::fmt::Debug for GrantedObsoleteSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}{}@{}",
            self.source.class_name,
            self.source.method_name,
            self.source.method_descriptor,
            self.stamp
        )
    }
}

impl PartialEq for GrantedObsoleteSource {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.source, &other.source) && self.stamp == other.stamp
    }
}

impl Eq for GrantedObsoleteSource {}

impl<'a> PollingBody<'a> {
    /// The body an artifact names, or `None` for one that was never published
    /// (`NO_OWNER_CLASS`) or whose label is not a method key (a lambda
    /// adapter's).
    pub(crate) fn from_artifact(owner_class_id: u32, label: &'a str) -> Option<Self> {
        if owner_class_id == cratonvm_types::jit_activation::NO_OWNER_CLASS {
            return None;
        }
        let (rest, descriptor) = label.rsplit_once(':')?;
        let (class_name, method_name) = rest.rsplit_once('.')?;
        if class_name.is_empty() || method_name.is_empty() || !descriptor.starts_with('(') {
            return None;
        }
        Some(Self {
            class_id: ClassId::new(owner_class_id),
            class_name,
            method_name,
            descriptor,
            deopt_points: (0, 0),
            exit_boxes: &[],
            install_epoch: None,
            withdrawn_by_redefinition: false,
            leaves_as_renumbered_obsolete: false,
            obsolete_source: None,
            caller_held_binding: None,
        })
    }

    /// This body with its artifact's caller-held binding mark (interpreter
    /// round i2 wave 53, lane L2; [`compiled_frame_exit_verdict`]).
    pub(crate) fn with_caller_held_binding(mut self, bound: bool) -> Self {
        self.caller_held_binding = Some(bound);
        self
    }

    /// This body with its artifact's redefinition withdrawal (wave 23, lane
    /// L6; [`polling_body_must_leave`]).
    pub(crate) fn with_withdrawn_by_redefinition(mut self, withdrawn: bool) -> Self {
        self.withdrawn_by_redefinition = withdrawn;
        self
    }

    /// This body with its artifact's own-class leave mark (wave 45, lane L2;
    /// [`withdrawn_body_may_leave`]).
    pub(crate) fn with_leaves_as_renumbered_obsolete(mut self, leaves: bool) -> Self {
        self.leaves_as_renumbered_obsolete = leaves;
        self
    }

    /// This body with the bytecode a stash sink must resume its exit in
    /// (wave 46, lane L2): a method-entry body forced to leave as its class's
    /// obsolete activation. Kept by the grant ([`granted_obsolete_source`]).
    pub(crate) fn with_obsolete_source(mut self, source: Option<ObsoleteEntrySource<'a>>) -> Self {
        self.obsolete_source = source;
        self
    }

    /// This body with the two facts of its artifact the exit sinks need
    /// (wave 18): the `[start, end)` addresses of its `deopt_points` and its
    /// `install_epoch`.
    pub(crate) fn with_artifact_facts(
        mut self,
        deopt_points: (usize, usize),
        install_epoch: u64,
    ) -> Self {
        self.deopt_points = deopt_points;
        self.install_epoch = Some(install_epoch);
        self
    }

    /// This body with its artifact's boxed points (wave 19, lane L2): an
    /// optimizing body's poll exits (`ir_lower::Lowerer::emit_poll_mode_exit`)
    /// are boxes outside `deopt_points`, so without them a grant could never
    /// match such a body's stash. Empty for a single-pass artifact.
    pub(crate) fn with_boxed_exit_points(
        mut self,
        boxes: &'a [Box<cratonvm_jit::deopt::DeoptimizationPoint>],
    ) -> Self {
        self.exit_boxes = boxes;
        self
    }

    /// Does a grant for this body name any point a sink could be handed?
    /// A non-empty `deopt_points` range, or an `OsrExit` among the boxes.
    fn exit_points_known(&self) -> bool {
        let (start, end) = self.deopt_points;
        start < end || self.boxed_exit_addresses().next().is_some()
    }

    /// The addresses of the boxed `OsrExit` points: the stashed point address
    /// of every exit an optimizing body's back-edge poll takes.
    fn boxed_exit_addresses(&self) -> impl Iterator<Item = usize> + 'a {
        let boxes: &'a [Box<cratonvm_jit::deopt::DeoptimizationPoint>] = self.exit_boxes;
        boxes
            .iter()
            .filter(|p| p.reason == cratonvm_jit::deopt::DeoptReason::OsrExit)
            // Cast: point addresses, compared only (`owns_deopt_point`).
            .map(|p| &**p as *const cratonvm_jit::deopt::DeoptimizationPoint as usize)
    }

    /// Was this body compiled after its VM's last code-cache flush and its
    /// class's last redefinition (`JitCache::compiled_since_redefinition_of`;
    /// a scoped redefinition flushes nothing since interpreter round i1 wave
    /// 20)? `false` when unknown.
    fn compiled_since_last_redefinition(&self, shared: &SharedVm) -> bool {
        self.install_epoch.is_some_and(|epoch| {
            shared
                .jit
                .jit_cache
                .compiled_since_redefinition_of(self.class_id, epoch)
        })
    }
}

/// The last mode exit the safepoint slow path granted a named body on this
/// thread (interpreter round i1 wave 18, lane L3): the VM, the body's class,
/// the address range of its `deopt_points` and its `install_epoch`.
///
/// The exit's stashed frame carries only a method key and the point that
/// fired, so a sink holding nothing else judged the exit by class NAME
/// ([`stashed_exit_left_for_the_interpreter`]) and, for a class whose name
/// resolves to another loader's class, would charge it as a failed
/// speculation; and the dispatch helpers' sink refused to resume any stash of
/// a redefined class (`helpers::try_resume_trapped_callee`), which re-ran the
/// callee from entry. The poll that granted the exit runs on the same thread
/// right before the body's stub stashes it, so the point address ties the
/// stash to this record. A record outlives its exit (it is replaced, not
/// consumed); a later stash matches it only through an address inside that
/// artifact's points and an `OsrExit` cause, and the currency of the body is
/// re-read at each use, so a stale record can only spare a charge or let a
/// sink resume a body that is still current.
///
/// Wave 19, lane L2: an optimizing body's poll exits are boxes outside its
/// `deopt_points`, so the record also keeps their addresses (`boxed_exits`,
/// sorted; empty for a single-pass body, which allocates nothing). They are
/// copied, never dereferenced, so the record needs no argument about the
/// artifact's lifetime. Until then an optimizing body got no record at all
/// and kept the wave-17 rule.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ModeExitGrant {
    vm: usize,
    class_id: ClassId,
    deopt_points: (usize, usize),
    boxed_exits: Box<[usize]>,
    install_epoch: Option<u64>,
    /// Wave 23, lane L6: granted because a class redefinition withdrew the
    /// body ([`withdrawn_body_may_leave`]), not because an agent needs its
    /// method interpreted.
    withdrawn: bool,
    /// Wave 46, lane L2: a method-entry body forced to leave as its class's
    /// obsolete activation -- the bytecode its exit must be resumed in
    /// ([`granted_obsolete_source`]).
    obsolete_source: Option<GrantedObsoleteSource>,
}

impl ModeExitGrant {
    /// Is `point_addr` one of the granted body's exit points?
    fn names_point(&self, point_addr: usize) -> bool {
        let (start, end) = self.deopt_points;
        (start..end).contains(&point_addr) || self.boxed_exits.binary_search(&point_addr).is_ok()
    }
}

/// What a sink learns from a [`ModeExitGrant`] that names its stash: the
/// granted body's class and `install_epoch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GrantedBody {
    class_id: ClassId,
    install_epoch: Option<u64>,
}

thread_local! {
    static MODE_EXIT_GRANT: std::cell::RefCell<Option<ModeExitGrant>> =
        const { std::cell::RefCell::new(None) };
}

/// Record that the slow path told `body` to leave (its polling-body bit is
/// set). Nothing for a body whose artifact facts are unknown.
fn note_mode_exit_grant(shared: &SharedVm, body: &PollingBody<'_>) {
    if !body.exit_points_known() {
        return;
    }
    // No allocation for a single-pass body (no boxed exit).
    let mut boxed_exits: Vec<usize> = body.boxed_exit_addresses().collect();
    boxed_exits.sort_unstable();
    let grant = ModeExitGrant {
        vm: shared.vm_identity,
        class_id: body.class_id,
        deopt_points: body.deopt_points,
        boxed_exits: boxed_exits.into_boxed_slice(),
        install_epoch: body.install_epoch,
        withdrawn: body.withdrawn_by_redefinition,
        obsolete_source: body.obsolete_source.map(|o| GrantedObsoleteSource {
            source: Arc::clone(o.source),
            stamp: o.stamp,
        }),
    };
    let _ = MODE_EXIT_GRANT.try_with(|g| {
        if let Ok(mut slot) = g.try_borrow_mut() {
            *slot = Some(grant);
        }
    });
}

/// The body of the grant [`note_mode_exit_grant`] recorded on this thread,
/// when the point at `point_addr` is one of that body's (0 never is).
fn mode_exit_grant_for(shared: &SharedVm, point_addr: usize) -> Option<GrantedBody> {
    if point_addr == 0 {
        return None;
    }
    MODE_EXIT_GRANT
        .try_with(|g| {
            let slot = g.try_borrow().ok()?;
            let grant = slot.as_ref()?;
            (grant.vm == shared.vm_identity && grant.names_point(point_addr)).then_some(
                GrantedBody {
                    class_id: grant.class_id,
                    install_epoch: grant.install_epoch,
                },
            )
        })
        .ok()
        .flatten()
}

/// Was the `OsrExit` stashed by the point at `point_addr` a mode exit the
/// safepoint slow path granted a body compiled after every class
/// redefinition of this VM (wave 18)? Such a body's bytecode is its class's
/// current bytecode, so a sink may resume its frame against the class as it
/// is now, although the class was redefined (earlier) — the exit is then not a
/// replay. `false` for any other cause, point or body.
pub(crate) fn granted_exit_of_a_current_body(
    shared: &SharedVm,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> bool {
    cause.is_some_and(|c| c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit)
        && mode_exit_grant_for(shared, point_addr).is_some_and(|grant| {
            grant.install_epoch.is_some_and(|epoch| {
                shared
                    .jit
                    .jit_cache
                    .compiled_since_redefinition_of(grant.class_id, epoch)
            })
        })
}

/// The bytecode, and its constant-pool generation, the `OsrExit` stashed by
/// the point at `point_addr` must be resumed in, when the safepoint slow path
/// granted that exit to a METHOD-ENTRY body forced to leave as its class's
/// obsolete activation after a renumbering redefinition (interpreter round i1
/// wave 46, lane L2; `cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`).
/// `None` for any other cause, point or body. The stash sinks that rebuild a
/// frame from their own template ask it
/// (`jit_bridge::granted_obsolete_activation_source`), and resume the frame
/// in this bytecode, restamped with this generation, instead of refusing a
/// frame of a redefined class and re-running the method from entry.
pub(crate) fn granted_obsolete_source(
    shared: &SharedVm,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> Option<(Arc<CachedBytecodeMethod>, u64)> {
    if point_addr == 0
        || !cause.is_some_and(|c| c.reason == cratonvm_jit::deopt::DeoptReason::OsrExit)
    {
        return None;
    }
    MODE_EXIT_GRANT
        .try_with(|g| {
            let slot = g.try_borrow().ok()?;
            let grant = slot.as_ref()?;
            if grant.vm != shared.vm_identity || !grant.names_point(point_addr) {
                return None;
            }
            let obsolete = grant.obsolete_source.as_ref()?;
            Some((Arc::clone(&obsolete.source), obsolete.stamp))
        })
        .ok()
        .flatten()
}

/// The verdict `jit_safepoint_slow_path` returns: the
/// `cratonvm_jit::SAFEPOINT_VERDICT_*` bits. `SAFEPOINT_VERDICT_INNERMOST_FRAME`
/// when the thread's innermost interpreter frame must run interpreted
/// ([`compiled_loop_must_leave`], or every compiled frame must; only an OSR
/// body acts on it). `SAFEPOINT_VERDICT_POLLING_BODY` (a method-entry body
/// acts on that bit alone) when the polling `body` must
/// ([`polling_body_must_leave`]), or, for a body the poll did not name, when
/// every compiled frame must ([`every_compiled_frame_may_leave`]). `thread` is
/// `None` on a thread with no `JvmThread`, which can only be told the VM-wide
/// answer about its innermost frame.
pub(crate) fn compiled_frame_exit_verdict(
    shared: &SharedVm,
    thread: Option<&JvmThread>,
    body: Option<PollingBody<'_>>,
) -> i64 {
    let every = every_compiled_frame_may_leave(shared);
    let innermost = every || thread.is_some_and(|thread| compiled_loop_must_leave(shared, thread));
    let mut innermost = innermost;
    let mut named = false;
    let polling = match body {
        Some(body) => {
            let leave = polling_body_must_leave(shared, body);
            if leave {
                // For the sinks that hold only the stash (wave 18).
                note_mode_exit_grant(shared, &body);
                // Wave 23, lane L6: a withdrawn OSR body tests the
                // innermost-frame bit, and the frame that bit is about is the
                // one it runs in. A method-entry body ignores the bit.
                innermost |= body.withdrawn_by_redefinition;
                // Interpreter round i2 wave 53, lane L2: the bit only a NAMED
                // body no compiled caller bound for a caller-held CALL gets
                // (a body of unknown binding never does), which the
                // optimizing tier's trap-free, call-free synchronized
                // method-entry bodies test (`SAFEPOINT_VERDICT_NAMED_BODY`).
                named = body.caller_held_binding == Some(false);
            }
            leave
        }
        None => every,
    };
    let mut verdict = 0;
    if innermost {
        verdict |= cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME;
    }
    if polling {
        verdict |= cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
    }
    if named {
        verdict |= cratonvm_jit::SAFEPOINT_VERDICT_NAMED_BODY;
    }
    verdict
}

/// Could any compiled frame of this VM be told to leave now? The negative
/// fast path of [`compiled_frame_exit_verdict`] (every bit it can set needs a
/// JVMTI interpreter-only event or an armed JDWP gate), asked by the slow path
/// before it looks up the polling body, and by
/// [`request_compiled_loop_exits`] before it pauses.
pub(crate) fn compiled_frames_may_be_asked_to_leave(shared: &SharedVm) -> bool {
    crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity)
        || super::breakpoints_armed_now(shared)
}

/// Must the compiled body that polled leave for the interpreter now, and may
/// it (wave 17)? Its method must run interpreted
/// (`jvmti_requires_interpreter_for_method`, the doors' question), and:
///
/// * it is not an OBSOLETE body: its class has not been redefined (by id or
///   by name, the two ways the resume sinks ask) since it was compiled.
///   `helpers::try_resume_trapped_callee` refuses a stash whose class name
///   was redefined and re-runs the callee from entry, which would replay what
///   the body committed; the per-class form of
///   [`every_compiled_frame_may_leave`]'s process-wide clause. Wave 18: a
///   body compiled after every redefinition of its VM
///   (`JitCache::compiled_since_redefinition_of`) runs its class's current
///   bytecode, and its exit is resumed (the sinks ask
///   [`granted_exit_of_a_current_body`]), so the clause holds only for a body
///   compiled before a redefinition. That body runs bytecode the class no
///   longer has, and no sink resumes it (each refuses a frame of a redefined
///   class, as above). Since wave 19 an interpreter frame running such a body
///   can be translated into the merged pool (`obsolete_frames`); a sink that
///   rebuilt the frame from the body's own bytecode and stamp would let the
///   exit go too; see
///   `docs/internal/fixed-bugs/interpreter-L3-obsolete-methods-keep-no-constant-pool-FIXED-20260925.md`.
///   Wave 46 built those sinks (they resume a granted exit in the body's
///   own bytecode, [`granted_obsolete_source`]), and since wave 47 (lane L2,
///   `cratonvm_jit::not_entrant::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`) a
///   method-entry body the safepoint verdict hands that bytecode
///   (`obsolete_source`, only once its frame is known to translate) passes
///   this clause;
/// * wave 17: when the answer is about this method only (not every method),
///   its class name resolves to its own class id, unless the body's artifact
///   facts are known (wave 18: the exit is then recorded
///   ([`note_mode_exit_grant`]) and a sink holding only the stashed frame
///   judges it by that record's class id,
///   [`stashed_exit_left_for_the_interpreter`], instead of by name, which
///   would charge it as a failed speculation).
fn polling_body_must_leave(shared: &SharedVm, body: PollingBody<'_>) -> bool {
    // Interpreter round i1 wave 23, lane L6: withdrawn by a class
    // redefinition, whether or not an agent listens.
    if body.withdrawn_by_redefinition && withdrawn_body_may_leave(shared, &body) {
        return true;
    }
    if !super::jvmti_requires_interpreter_for_method(
        shared,
        body.class_id,
        body.method_name,
        body.descriptor,
    ) {
        return false;
    }
    // Interpreter round i1 wave 47, lane L2: except a body whose grant will
    // carry the bytecode it was compiled from (`obsolete_source`, set by the
    // safepoint verdict only for a method-entry body whose frame translates;
    // `cratonvm_jit::not_entrant::INTERPRETER_ONLY_OBSOLETE_BODIES_LEAVE`):
    // the sinks resume that exit in its own bytecode instead of re-running it.
    if body.obsolete_source.is_none()
        && (crate::runtime::redefine_state::class_was_redefined(shared, body.class_id)
            || crate::runtime::redefine_state::named_class_was_redefined(shared, body.class_name))
        && !body.compiled_since_last_redefinition(shared)
    {
        return false;
    }
    body.exit_points_known()
        || every_compiled_frame_must_leave(shared)
        || shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(body.class_name)
            == Some(body.class_id)
}

/// May a body a class redefinition withdrew
/// (`CompiledMethod::is_withdrawn_by_redefinition`) leave for the interpreter
/// at this poll (interpreter round i1 wave 23, lane L6;
/// `docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md`)?
///
/// Such a body may be running a splice of the redefined class's OLD bytecode
/// (an inlined callee), and every later iteration of its loop would run it
/// again; HotSpot deoptimizes the frame
/// (`Deoptimization::deoptimize_all_marked`). The exit resumes the frame
/// interpreted at the poll's own bci, and its next call reaches the new
/// bytecode. Both tiers take a poll's mode exit only at a back edge of the
/// method's OWN code, never inside a splice (`x64/safepoint.rs`'s
/// `mode_exit_target` family refuses an inline scope, the IR tier's
/// `back_edge_mode_exit_state` a spliced header), so the resumed frame is
/// always the polling method's.
///
/// Refused when the sinks could not resume it: no exit point is known, or the
/// body is an OBSOLETE body -- its own class was redefined since it was
/// compiled (the same clause as [`polling_body_must_leave`]'s, for the same
/// reason: a sink refuses to resume a frame of a redefined class and would
/// re-run the method, replaying what it committed). Such a body is the
/// redefined method's own obsolete activation, which JEP 109 lets finish on
/// its old bytecode anyway.
///
/// Interpreter round i1 wave 45, lane L2: except an OSR body the JIT forced as
/// its own class's obsolete activation after a redefinition that renumbered
/// the class's pool (`leaves_as_renumbered_obsolete`, set only with
/// `cratonvm_jit::not_entrant::OWN_CLASS_RENUMBERED_OSR_BODIES_LEAVE` on). No
/// stash sink resumes such an exit: the OSR door transfers it into its own
/// live interpreter frame in place (`try_osr`), a frame the obsolete-frame
/// machinery moves onto its translated body before it runs another bytecode,
/// so nothing is replayed and the rest of the activation runs its old
/// bytecode against the old constants, interpreted.
///
/// Wave 46, lane L2: and a METHOD-ENTRY body of that kind
/// (`OWN_CLASS_RENUMBERED_ENTRY_BODIES_LEAVE`), whose mark the safepoint
/// slow path passes on only once the frame it would stash is known to
/// translate (`helpers::jit_safepoint_loop_exit_verdict`). Its exit is
/// stashed; the grant keeps the bytecode it was compiled from
/// ([`granted_obsolete_source`]), and the stash sinks resume it there,
/// restamped with its pool generation, instead of re-running the method.
fn withdrawn_body_may_leave(shared: &SharedVm, body: &PollingBody<'_>) -> bool {
    body.exit_points_known()
        && (body.leaves_as_renumbered_obsolete
            || !((crate::runtime::redefine_state::class_was_redefined(shared, body.class_id)
                || crate::runtime::redefine_state::named_class_was_redefined(
                    shared,
                    body.class_name,
                ))
                && !body.compiled_since_last_redefinition(shared)))
}

/// Was the last mode exit the safepoint slow path granted on this thread one
/// for a body of `class_id` that a class redefinition withdrew (wave 23, lane
/// L6)? What [`exit_left_for_the_interpreter`] adds for such an exit, so the
/// sinks resume it uncharged: the exit says nothing about the code's
/// speculation. Like every grant it can outlive its exit; a stale one can
/// only spare a later `OsrExit` of the same class its charge.
fn withdrawn_exit_granted_for(shared: &SharedVm, class_id: ClassId) -> bool {
    MODE_EXIT_GRANT
        .try_with(|g| {
            g.try_borrow().ok().is_some_and(|slot| {
                slot.as_ref().is_some_and(|grant| {
                    grant.withdrawn && grant.vm == shared.vm_identity && grant.class_id == class_id
                })
            })
        })
        .unwrap_or(false)
}

/// Make every compiled body running in this VM take its back-edge poll's slow
/// path once, after a class redefinition withdrew compiled bodies (interpreter
/// round i1 wave 23, lane L6): a frame running a withdrawn body then leaves
/// for the interpreter ([`withdrawn_body_may_leave`]).
///
/// Taken on the redefining thread `requester` itself, as
/// `obsolete_frames::after_redefinition` takes its handshake, so that by the
/// time `RetransformClasses` / `RedefineClasses` returns every peer that
/// polled has parked and, on release, read a verdict that sends a withdrawn
/// body's frame to the interpreter before its next iteration -- HotSpot
/// deoptimizes those frames inside the redefinition's own safepoint. (The
/// agent-mode pause, [`request_compiled_loop_exits`], runs on a thread of its
/// own because its callers hold locks a mutator may need; a redefinition
/// holds none by now.) A peer the pause had to freeze before its poll passes
/// none during it, so then the same bounded, doubling retries run on a
/// short-lived thread. Returns whether a pause was taken or requested.
///
/// Interpreter round i1 wave 26, lane L6 (stage 2 of
/// `i25-L6-proposal-retire-the-loop-exit-retries-for-forced-bodies`): no
/// retries when the exit polls of every body withdrawn since `withdrawn_before`
/// (the caller's `JitCache::bodies_withdrawn_by_redefinition` snapshot) were
/// forced (`JitCache::every_withdrawal_forced_since`): a peer the first pause
/// froze then leaves at its next exit-capable back edge on its own, and each
/// retry would stop every mutator again for nothing. The first pause is kept
/// for HotSpot's ordering (no peer that polls during it is still running a
/// withdrawn body when the redefinition returns).
pub(crate) fn request_withdrawn_body_exits(
    shared: &SharedVm,
    requester: crate::ThreadId,
    withdrawn_before: u64,
) -> bool {
    // The withdrawal marks are published before this runs (the redefinition
    // set them under the cache's mutation lock); a poll that misses the pause
    // reads them at its next slow path.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    // Interpreter round i1 wave 24, lane L6: no pause when no other thread
    // can be running a compiled body -- the common case for a batch of
    // retransforms from an agent's own thread while the application waits.
    // The pause only reaches frames already running; a frame that starts
    // after this read enters through a door the redefinition already closed
    // (a patched entry, an expired memo, and the OSR door, which counts its
    // body running before it asks whether it was withdrawn --
    // `osr_bodies_running` and `try_osr`, SeqCst on both sides). A body whose
    // patch was refused stays enterable either way.
    if !crate::jit::conservative_roots::peer_threads_hold_jit_entries()
        && !super::jit_bridge::osr_bodies_running(shared)
    {
        return false;
    }
    match super::NonMovingPause::request_handshake(
        shared,
        requester,
        super::gc_events::NonCollectionPause::LoopExit,
        LOOP_EXIT_GRACE,
    ) {
        Some(pause) => {
            let froze = pause.froze_peers();
            drop(pause);
            if !froze {
                return true;
            }
        }
        // Another pause owns the world: its polls take the slow path and
        // read the verdict after it.
        None => return true,
    }
    if shared
        .jit
        .jit_cache
        .every_withdrawal_forced_since(withdrawn_before)
    {
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] loop-exit retries skipped: the pause froze a peer, and every \
                 withdrawn body's exit polls are forced"
            );
        }
        return true;
    }
    let Some(owner) = shared.try_get_arc() else {
        return true;
    };
    std::thread::Builder::new()
        .name("cratonvm-withdrawn-exit-pause".to_string())
        .spawn(move || {
            let mut grace = LOOP_EXIT_GRACE;
            for _ in 0..LOOP_EXIT_ATTEMPTS {
                // An initiator no registry entry carries (wave 58, lane L6:
                // `PAUSE_OUTSIDER` says so to the census).
                let Some(pause) = super::NonMovingPause::request_handshake(
                    &owner,
                    super::PAUSE_OUTSIDER,
                    super::gc_events::NonCollectionPause::LoopExit,
                    grace,
                ) else {
                    return;
                };
                let froze = pause.froze_peers();
                drop(pause);
                if !froze {
                    return;
                }
                grace = grace.saturating_mul(2);
            }
        })
        .is_ok()
}

/// Must EVERY compiled frame of this VM leave for the interpreter, whatever its
/// method — and may it? A JVMTI interpreter-only event, or a JDWP request that
/// concerns every method (a step, a suspension, a field watch, an exception
/// request), while no class has been redefined.
///
/// The redefinition clause is what keeps an exit from being a replay. After a
/// redefinition a method-entry body still running is an obsolete body: the
/// dispatch helpers' sink refuses to resume a frame of a redefined class
/// (`helpers::try_resume_trapped_callee`) and re-runs the callee from entry,
/// and the interpreter door's sink does the same for a superseded body of one
/// (`real_frame_deopt_resume_and_despeculate`), so leaving would commit what
/// the body had already done a second time. The flag is the process-wide
/// negative fast path (`classloading::any_class_redefined`): coarse, and it
/// only ever withholds an exit. OSR bodies are not affected: they transfer
/// into their own live frame and keep leaving on
/// [`compiled_loop_must_leave`].
pub(crate) fn every_compiled_frame_may_leave(shared: &SharedVm) -> bool {
    every_compiled_frame_must_leave(shared) && !crate::classloading::any_class_redefined()
}

/// Must every compiled frame of this VM run interpreted, whatever its method?
/// The VM-wide half of `jvmti_requires_interpreter_for_method`, plus the JDWP
/// requests that concern every method.
fn every_compiled_frame_must_leave(shared: &SharedVm) -> bool {
    crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity)
        || debugger_requires_every_method(shared)
}

/// The JDWP half of [`every_compiled_frame_must_leave`].
#[cfg(feature = "experimental-debug")]
#[inline(always)]
fn debugger_requires_every_method(shared: &SharedVm) -> bool {
    shared
        .debug
        .breakpoints_active
        .load(std::sync::atomic::Ordering::Relaxed)
        && shared
            .debug
            .debugger_gates
            .requires_interpreter_everywhere()
}

/// [`debugger_requires_every_method`] in a build without the JDWP surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
fn debugger_requires_every_method(_shared: &SharedVm) -> bool {
    false
}

/// Was a deopt of `reason` in the method `class_id` / `method_name` /
/// `descriptor` an exit the interpreter-only mode asked for, rather than a
/// failed speculation? An `OsrExit` while that method must run interpreted
/// (`jvmti_requires_interpreter_for_method`).
///
/// Outside an OSR body an `OsrExit` point is reached only by a back-edge or
/// self-tail poll leaving on the every-frame verdict, or by the deopt-osr test
/// triggers, which do not coincide with an agent's request; inside one, only
/// by those polls and the same triggers. Every charging sink asks this and
/// declines the charge and the per-bci de-speculation on `true`
/// (`DeoptimizationController::deoptimize_in`,
/// `helpers::despeculate_trapped_method`,
/// `deopt_resume::real_frame_deopt_resume_and_despeculate`): an agent's
/// request says nothing about the code, and charging it would evict the body
/// and, at half the per-method budget of exits at one bci, make the method
/// not compilable for the rest of the run. Interpreter round i1 wave 15, lane
/// L3.
pub(crate) fn exit_left_for_the_interpreter(
    shared: &SharedVm,
    reason: cratonvm_jit::deopt::DeoptReason,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
) -> bool {
    reason == cratonvm_jit::deopt::DeoptReason::OsrExit
        && (super::jvmti_requires_interpreter_for_method(shared, class_id, method_name, descriptor)
            // Wave 23, lane L6: the exit a redefinition's withdrawal asked
            // for (`withdrawn_body_may_leave`).
            || withdrawn_exit_granted_for(shared, class_id))
}

/// [`exit_left_for_the_interpreter`] for a stashed frame, by the cause the
/// stash kept beside it (`None`: not an exit), for a sink that holds the frame
/// and the address of the point that stashed it (`point_addr`, 0 when
/// unknown) but not its method's class id (a frame another method stashed).
///
/// The class is the one the safepoint slow path recorded when it granted the
/// exit, when the point is that body's ([`note_mode_exit_grant`], wave 18);
/// otherwise it is looked up by name, and id 0 (two loaders define it) still
/// answers the VM-wide half. Judged by name alone, the exit of a body whose
/// class name resolves to another loader's class was charged.
pub(crate) fn stashed_exit_left_for_the_interpreter(
    shared: &SharedVm,
    rframe: &cratonvm_jit::deopt::ReconstructedFrame,
    cause: Option<cratonvm_jit::deopt::DeoptCause>,
    point_addr: usize,
) -> bool {
    let Some(cause) = cause else {
        return false;
    };
    if cause.reason != cratonvm_jit::deopt::DeoptReason::OsrExit {
        return false;
    }
    let Some((rest, descriptor)) = rframe.method_key.rsplit_once(':') else {
        return false;
    };
    let Some((class_name, method_name)) = rest.rsplit_once('.') else {
        return false;
    };
    let class_id = match mode_exit_grant_for(shared, point_addr) {
        Some(grant) => grant.class_id,
        None => shared
            .classes
            .class_manager
            .read()
            .get_loaded_class_id(class_name)
            .unwrap_or(ClassId::new(0)),
    };
    exit_left_for_the_interpreter(shared, cause.reason, class_id, method_name, descriptor)
}

/// Make every OSR body running in this VM take its back-edge poll's slow path
/// once, so a loop that must now run interpreted
/// ([`compiled_loop_must_leave`]) leaves compiled code. Called on the edge
/// where interpreter-only mode comes into force, after the mode is published
/// (`jvmti::publish_union_listener_flags`, `debug::publish_debugger_gates`).
/// Returns whether a pause was requested.
///
/// The polls test the VM's stop-the-world byte, so the request is a
/// non-collection pause of its own kind (`NonMovingPause::request_handshake`,
/// `door=loop-exit` under `--verbose:gc`; nothing moves, nothing is
/// collected). Wave 13: it waits a cooperative grace slice
/// ([`LOOP_EXIT_GRACE`]) before the take-over may freeze a peer, because a
/// peer frozen in compiled code before its poll passes none during the pause —
/// a loop whose iteration is dominated by a compiled callee used to be frozen
/// mid-iteration and stay compiled. A pause that still froze a peer is
/// repeated with a doubled slice while an OSR body is running, at most
/// [`LOOP_EXIT_ATTEMPTS`] times in all, so a peer that never polls
/// (`CRATONVM_JIT_SAFEPOINT_POLLS=0`, an intrinsic spin) costs a bounded
/// number of bounded pauses.
///
/// The pause is taken on a short-lived thread of its own, never on the
/// caller's: the JDWP publisher holds the debug-state lock, which a mutator may
/// need before it can reach a safepoint, and a JVMTI caller can be anywhere in
/// agent code. So the exit is asynchronous — the loop leaves within one pause,
/// not before the caller returns.
///
/// Nothing is requested when no single-pass OSR body of this VM is running
/// (`jit_bridge::osr_bodies_running`) and no compiled frame can be told to
/// leave ([`compiled_frames_may_be_asked_to_leave`]; wave 15: method-entry
/// bodies are not counted) — the common case, and the one every agent-free
/// run and unit test is in. Since wave 17 a method-entry body leaves on an
/// answer about its own method, so a JDWP breakpoint (a per-method set that
/// grew) or a mode that comes into force after a redefinition takes the pause
/// too; HotSpot's breakpoint change is a safepoint operation as well. What
/// this does not reach:
///
/// * a body whose door asked the mode before it was published but that calls
///   into the body only after the pause ended (its count was in, so it is
///   the rare thread descheduled between the door's re-ask and the entry) —
///   it leaves at the next pause of any kind;
/// * the bodies no poll verdict reaches: the optimizing (IR) tier except the
///   unconditional back edges of an OSR body (wave 15, lane L2:
///   `ir_lower::Lowerer::back_edge_mode_exit_state`; such a body is counted in
///   `osr_bodies_running` too) and of a method-entry body whose method every
///   stash sink resumes (wave 18, lane L2: `ir_lower::ir_entry_mode_exits_admitted`
///   — not `synchronized`; since wave 19 a method with a `synchronized` block
///   too, since wave 22 an `ACC_SYNCHRONIZED` one unless its graph can neither
///   trap nor call), back edges the single-pass admission refuses
///   (`mode_exit_target` / `branch_mode_exit_target` / `self_tail_mode_exit_target` in
///   `jit/src/x64/safepoint.rs`), a method-entry body whose poll does not
///   name it (no compile id was reserved for it) under a JDWP request that
///   concerns only some methods (a breakpoint) or after a class redefinition,
///   and a named body compiled before its class was redefined (an obsolete
///   body; wave 18: one compiled after the redefinition leaves).
pub(crate) fn request_compiled_loop_exits(shared: &SharedVm) -> bool {
    // The caller published the mode before calling; the OSR door counts a
    // body in before entering it. With both sides fenced, a body that misses
    // the mode is one this read sees.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    // Wave 15: method-entry bodies leave on the polling-body verdict too, and
    // nothing counts them (a count would cost every compiled call), so a mode
    // that may concern a compiled frame takes the pause whatever runs (wave
    // 17: about each polling body's own method). Both callers ask only on the
    // edge where what the mode concerns grew.
    if !super::jit_bridge::osr_bodies_running(shared)
        && !compiled_frames_may_be_asked_to_leave(shared)
    {
        return false;
    }
    // A `SharedVm` not built by `Vm::new` (unit fixtures) has no owning handle
    // to give the thread.
    let Some(owner) = shared.try_get_arc() else {
        return false;
    };
    std::thread::Builder::new()
        .name("cratonvm-loop-exit-pause".to_string())
        .spawn(move || {
            let mut grace = LOOP_EXIT_GRACE;
            for _ in 0..LOOP_EXIT_ATTEMPTS {
                // An initiator no registry entry carries: this thread is not
                // a mutator, so the census counts every mutator of the VM.
                // `None` (another pause already owns the world) is fine too:
                // the polls take the slow path for that one, and the verdict
                // is read after the park. Wave 58 (lane L6): `PAUSE_OUTSIDER`
                // says so to the census, and a thread in a running native is
                // excused (`CRATONVM_HANDSHAKE_NATIVE_EXCUSE`).
                let Some(pause) = super::NonMovingPause::request_handshake(
                    &owner,
                    super::PAUSE_OUTSIDER,
                    super::gc_events::NonCollectionPause::LoopExit,
                    grace,
                ) else {
                    return;
                };
                let froze = pause.froze_peers();
                drop(pause);
                if !loop_exit_pause_again(froze, super::jit_bridge::osr_bodies_running(&owner)) {
                    return;
                }
                grace = grace.saturating_mul(2);
            }
        })
        .is_ok()
}

/// [`request_compiled_loop_exits`] again, for a JDWP suspension still waiting
/// for a thread to stop (interpreter round i2 wave 53, lane L6;
/// `debug::await_suspension_settled`), but only while compiled code may be
/// what keeps it running: a single-pass OSR body of this VM is counted in, or
/// some thread is inside a JIT call. Answers whether a pause was requested.
///
/// The suspension's own request, on the edge where the mode rose, is one
/// asynchronous attempt of at most [`LOOP_EXIT_ATTEMPTS`] pauses, and two
/// threads it misses run compiled for as long as nothing else pauses the VM:
/// a body whose OSR door counted in before the mode was published but entered
/// it only after the pause (a thread descheduled between the two, the gap
/// [`request_compiled_loop_exits`] documents), and a peer the take-over froze
/// in every attempt because it was descheduled for each grace slice (a loaded
/// host). Neither passes another poll's slow path, so a spinning loop the
/// debugger suspended ran on after the reply (`thread stable: false` in
/// `L1W49JdiSuspendCompletesBeforeReply`, 1 run in 41 on a loaded host). The
/// waiter asks again while it waits. Without compiled code the request is not
/// made: its pause would also wait for a thread running a long native, which
/// the waiter cannot help.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn request_compiled_loop_exits_again(shared: &SharedVm) -> bool {
    if !super::jit_bridge::osr_bodies_running(shared)
        && !crate::jit::conservative_roots::any_thread_in_jit()
    {
        return false;
    }
    request_compiled_loop_exits(shared)
}

/// Kill switch for the interpreter-only withdrawal (interpreter round i1 wave
/// 37, lane L1; [`note_every_method_needs_the_interpreter`]). `false`: a
/// request that needs every method interpreted only makes the doors stand
/// down and asks the loop exits, the behaviour of waves 12-36 (a compiled
/// caller's baked call into a compiled callee, and a frame running compiled
/// on a thread no pause reached, stay compiled).
pub(crate) const EVERY_BODY_WITHDRAWAL_ENABLED: bool = true;

/// A request that needs EVERY method interpreted came into force (`armed`)
/// or went, from `source` (`debug::WITHDRAWAL_BY_JDWP`: a JDWP step or
/// method event request; `debug::WITHDRAWAL_BY_JVMTI`: a JVMTI
/// `MethodEntry` / `MethodExit` / `SingleStep` / `FramePop` listener;
/// `debug::WITHDRAWAL_BY_JDK_BREAKPOINT`, wave 40: a breakpoint in a JDK
/// class's method, [`JDK_BREAKPOINT_INTERPRETER_ONLY_ENABLED`]).
/// Interpreter round i1 wave 37, lane L1: the "Wave 29 note" of
/// `docs/internal/proposals/interpreter/i18-L1-proposal-per-thread-interpreter-only-mode-20260925.md`.
///
/// The doors already refuse NEW entries into compiled code while such a
/// request is in force, and the loop-exit pause
/// ([`request_compiled_loop_exits`]) sends a polling loop back. What kept
/// running compiled is a compiled caller's baked `CALL` into a compiled
/// callee (and every callee it inlined), and a frame whose thread passed no
/// poll during the pause (blocked, or inside a long call): a method event of
/// such a callee was never posted, and a step never entered it. HotSpot
/// deoptimizes into its interpreter-only mode. Here, on the edge where the
/// first source arms, every compiled body of the VM is withdrawn, made not
/// entrant and has its exits forced
/// (`JitRealm::withdraw_every_body_for_the_interpreter`, the machinery a
/// whole-cache redefinition uses): a baked call re-dispatches through the
/// interpreter's invoke path (the stub does not forward while an agent may
/// need the interpreter, `jit_not_entrant_entry`), and a running frame leaves
/// at its next back edge or post-call exit ([`withdrawn_body_may_leave`]),
/// uncharged ([`exit_left_for_the_interpreter`]). While any source is armed
/// the interpreter's tier-up strides offer nothing to compile
/// (`jit_bridge::offer_invocation_to_tiered_manager`); when the last goes,
/// compiling resumes and the hot methods are compiled again, as on HotSpot.
///
/// Nothing on any per-call path: the callers are the JDWP gate publication
/// and the JVMTI listener publication, both off every hot path, and this
/// acts only on an edge of the per-VM bits.
///
/// Wave 39 (lane L1,
/// `docs/internal/fixed-bugs/interpreter-L1-each-jdwp-step-withdraws-every-compiled-body-FIXED-20261003.md`):
/// a debugger creates and deletes a step request per step, so each step used
/// to be a whole-cache flush and a recompilation of the hot set. Now, when
/// the last source goes, the tier-up strides stay closed for
/// `debug::WITHDRAWAL_LINGER_MS` more (`DebuggerGates::tier_up_held`), and
/// an arm that finds nothing compiled since the last withdrawal
/// ([`nothing_compiled_since_the_last_withdrawal`]) takes no second flush:
/// every body it would withdraw is withdrawn already.
#[cfg(feature = "experimental-debug")]
pub(crate) fn note_every_method_needs_the_interpreter(shared: &SharedVm, source: u8, armed: bool) {
    if !EVERY_BODY_WITHDRAWAL_ENABLED {
        return;
    }
    let gates = &shared.debug.debugger_gates;
    let before = gates.set_withdrawal_source(source, armed);
    if !armed {
        if before == source {
            // The last source went: the gate opens once the linger is over.
            gates.begin_withdrawal_linger();
        }
        return;
    }
    if before != 0 {
        // Another source already holds the code withdrawn: nothing was
        // compiled since (the tier-up strides offered nothing, and the flush
        // expired the queued requests).
        return;
    }
    let lingering = gates.end_withdrawal_linger();
    let source_name = match source {
        crate::debug::WITHDRAWAL_BY_JDWP => "jdwp",
        crate::debug::WITHDRAWAL_BY_JDK_BREAKPOINT => "jdk-breakpoint",
        _ => "jvmti",
    };
    if nothing_compiled_since_the_last_withdrawal(shared) {
        // Every body there is was withdrawn by the last withdrawal; what is
        // left is a compile not yet published (begun, or queued by a door
        // during the linger), which the fence and the code-state epoch keep
        // from publishing, as the withdrawal would.
        shared.jit.fence_compiles_for_the_interpreter();
        gates.note_withdrawal_held();
        // The positive control of the stepping session: one line per arm
        // that needed no flush (one `interpreter-only withdrawal:` line per
        // session, one of these per further step).
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] interpreter-only withdrawal held: source={source_name} \
                 lingering={lingering} held={} (nothing compiled since the last withdrawal)",
                gates.withdrawals_held(),
            );
        }
        return;
    }
    arm_this_vms_not_entrant_hook(shared);
    let (evicted, patched, forced) = shared.jit.withdraw_every_body_for_the_interpreter();
    gates.note_withdrawal_generation(shared.jit.jit_cache.generation());
    // The positive control: one line per withdrawal. The passes it runs print
    // their own `not-entrant pass:` and `exit polls forced: ...
    // redefined=<interpreter-only>` lines, and `CRATONVM_DBG_DEOPT=1` shows
    // each running body told to leave (`withdrawn body told to leave:`).
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] interpreter-only withdrawal: source={source_name} evicted={evicted} \
             not-entrant={patched} exits-forced={forced}",
        );
    }
}

/// Was nothing compiled in this VM since its last whole-cache withdrawal
/// (interpreter round i1 wave 39, lane L1)? Then a new arm of the
/// interpreter-only withdrawal has nothing to withdraw: every body it would
/// list was evicted, made not entrant and had its exits forced by that
/// withdrawal. `true` when:
///
/// * a withdrawal ran (`DebuggerGates::withdrawal_generation` is not `0`)
///   and the code cache's generation still reads what it left: every `put`,
///   `put_osr`, eviction and redefinition moves it;
/// * every optimizing OSR body the memo holds, and every live body the cache
///   never published, is marked withdrawn: those the cache does not publish,
///   so they do not move its generation.
///
/// A compile not yet published (begun during the linger, or queued by a
/// door that does not ask the strides) is kept from publishing by the
/// caller's fence (`JitRealm::fence_compiles_for_the_interpreter`: the flush
/// barrier and the code-state epoch, as the withdrawal raises them); one that
/// still publishes moves the generation, so the next arm withdraws it.
#[cfg(feature = "experimental-debug")]
fn nothing_compiled_since_the_last_withdrawal(shared: &SharedVm) -> bool {
    let left = shared.debug.debugger_gates.withdrawal_generation();
    left != 0
        && shared.jit.jit_cache.generation() == left
        && shared
            .jit
            .bridge
            .osr_optimizing_bodies()
            .iter()
            .all(|body| body.is_withdrawn_by_redefinition())
        && shared
            .jit
            .jit_cache
            .live_unpublished_exit_poll_bodies()
            .iter()
            .all(|body| body.is_withdrawn_by_redefinition())
}

/// Arm this VM's JIT cache with its not-entrant helper, so a withdrawal
/// patches the bodies it lists. Idempotent; the addresses are this VM's own,
/// as the redefinition path arms them (`redefine_class_with`).
/// A C JVMTI env bound while compiled code held lambda inline-cache thunks
/// (the i54-L1 retirement): a thunk enters a lambda's implementation from
/// compiled code with no Rust on the path, so a call through it records no
/// proxy frame. Every body is withdrawn, as for the interpreter-only mode
/// (`JitRealm::withdraw_every_body_for_the_interpreter`); recompiled while
/// proxy frames are recorded, a site installs no thunk
/// (`jit::helpers::try_lambda_site_direct_call`). HotSpot deoptimizes too
/// when a late agent takes such capabilities. `CRATONVM_DBG_JITC=1` prints
/// `[cratonvm-jitc] lambda proxy frames withdrawal: ...`.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn withdraw_bodies_for_lambda_proxy_frames(shared: &SharedVm) {
    arm_this_vms_not_entrant_hook(shared);
    let (evicted, patched, forced) = shared.jit.withdraw_every_body_for_the_interpreter();
    if crate::runtime::env_cache::dbg_jitc() {
        eprintln!(
            "[cratonvm-jitc] lambda proxy frames withdrawal: evicted={evicted} not-entrant={patched} exits-forced={forced}",
        );
    }
}

#[cfg(feature = "experimental-debug")]
fn arm_this_vms_not_entrant_hook(shared: &SharedVm) {
    shared
        .jit
        .jit_cache
        .arm_not_entrant(cratonvm_jit::NotEntrantHook {
            // Cast: a helper's address, baked into generated code.
            helper: crate::jit::helpers::jit_not_entrant_entry as *const () as usize,
            // Cast: this VM's address, the helpers' `vm_ptr` word.
            vm_ptr: shared as *const SharedVm as usize,
        });
}

/// Kill switch for the breakpoint's scoped withdrawal (interpreter round i1
/// wave 38, lane L1; [`note_breakpoint_classes_gained`]). `false`: a new
/// breakpoint only makes the doors refuse its method and asks the loop exits,
/// the behaviour of waves 7-37 (a compiled caller's baked call into the
/// method's compiled body, or a caller that inlined it, misses it).
#[cfg(feature = "experimental-debug")]
pub(crate) const BREAKPOINT_SCOPED_WITHDRAWAL_ENABLED: bool = true;

/// Methods of the classes `classes` gained a breakpoint (JDWP or JVMTI) at
/// this publication of the debugger gates: withdraw, per class, the compiled
/// bodies that would keep the breakpoint from being hit
/// (`JitRealm::withdraw_class_for_the_interpreter`; interpreter round i1 wave
/// 38, lane L1, the scoped withdrawal of
/// `docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md`).
///
/// The doors already refuse a NEW entry into the breakpoint's method, and the
/// loop-exit pause sends a polling loop of it back. What kept running compiled
/// was a compiled caller's baked `CALL` into the method's compiled body, a
/// caller that inlined it, and a frame whose thread passed no poll during the
/// pause (blocked in `Thread.sleep`, say): the breakpoint was never hit. Here
/// the class's own bodies and every body that inlined one of its methods or
/// copied its bytecode is evicted, made not entrant and has its exits forced,
/// and the bodies of every other class stay compiled -- one scan of the code
/// cache per class, on the edge where one of its methods gains a breakpoint,
/// not a whole-cache flush per breakpoint.
///
/// `classes`: each class's id and internal name. `None` for a name the caller
/// cannot tell without the class manager (a JVMTI breakpoint in a class the
/// debugger session never listed): the inlined-method records name classes by
/// name, so such a class takes the whole-cache withdrawal instead, once.
///
/// Nothing while a step or method event request holds every body withdrawn
/// ([`note_every_method_needs_the_interpreter`]): nothing is compiled then.
/// Off every hot path: the only caller is `debug::publish_debugger_gates`.
#[cfg(feature = "experimental-debug")]
pub(crate) fn note_breakpoint_classes_gained(
    shared: &SharedVm,
    classes: &[(ClassId, Option<String>)],
) {
    if !BREAKPOINT_SCOPED_WITHDRAWAL_ENABLED
        || classes.is_empty()
        || shared.debug.debugger_gates.every_body_withdrawn()
    {
        return;
    }
    arm_this_vms_not_entrant_hook(shared);
    let dbg = crate::runtime::env_cache::dbg_jitc();
    for (class_id, class_name) in classes {
        // Interpreter round i6 wave 117, lane L1
        // (`CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET`): a JDK class the JIT
        // cannot expand at a call site is withdrawn by class too.
        let (evicted, patched, forced, scoped) = match class_name.as_deref() {
            Some(name) if !shared.debug.debugger_gates.breakpoint_may_be_expanded(name) => shared
                .jit
                .withdraw_class_for_the_interpreter(*class_id, name),
            _ => {
                let (evicted, patched, forced) =
                    shared.jit.withdraw_every_body_for_the_interpreter();
                (evicted, patched, forced, false)
            }
        };
        // The positive control: one line per class. The passes it runs print
        // their own `not-entrant pass:` and `exit polls forced: ...
        // redefined=<breakpoint>` (or `<interpreter-only>` when the whole
        // cache was withdrawn) lines.
        if dbg {
            eprintln!(
                "[cratonvm-jitc] breakpoint withdrawal: class={} id={} scoped={scoped} \
                 evicted={evicted} not-entrant={patched} exits-forced={forced}",
                class_name.as_deref().unwrap_or("<unnamed>"),
                class_id.as_u32(),
            );
        }
        if !scoped {
            // Every body is withdrawn: the other classes have nothing left.
            return;
        }
    }
}

/// Kill switch for the interpreter-only hold of a breakpoint in a JDK class's
/// method (interpreter round i1 wave 40, lane L1; the remainder of
/// `docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md`).
/// While such a breakpoint stands, `debug::publish_debugger_gates` keeps every
/// method interpreted (`DebuggerGates::requires_interpreter`, so no door
/// enters a compiled body and no loop is OSR'd) and holds the tier-up strides
/// closed (`debug::WITHDRAWAL_BY_JDK_BREAKPOINT`, through
/// [`note_every_method_needs_the_interpreter`], which also withdraws every
/// body on its rising edge). The JIT's call-site intrinsics
/// (`try_resolve_*_intrinsic`) expand `String.length`, `Math.max`, the boxing
/// methods, ... in a caller from a fixed table, so the per-method refusal
/// ([`breakpoint_bars_compiling`]) cannot keep a caller compiled after the
/// breakpoint from missing it. `false`: waves 38-39's behaviour (the
/// one-shot whole-cache withdrawal when the breakpoint is set, and a caller
/// compiled later expands the method again).
#[cfg(feature = "experimental-debug")]
pub(crate) const JDK_BREAKPOINT_INTERPRETER_ONLY_ENABLED: bool = true;

/// Must no compile bind, splice or build a body of the method `name` +
/// `descriptor` declared by `declaring_class_id`, because a JDWP or JVMTI
/// breakpoint sits in it (interpreter round i1 wave 39, lane L1; item 3 of
/// `docs/internal/fixed-bugs/interpreter-L1-a-breakpoint-in-a-compiled-callee-of-a-running-compiled-caller-is-missed-FIXED-20261004.md`)?
///
/// The scoped withdrawal ([`note_breakpoint_classes_gained`]) takes away the
/// bodies compiled BEFORE the breakpoint was set; a caller compiled after it
/// could still splice the method (the inline resolver) or bake a raw CALL
/// into a body of it compiled after it (the tier-up stride offered it, or a
/// direct bind compiled it eagerly), and miss every later hit. Asked by:
///
/// * the interpreter's tier-up stride (`jit_bridge::offer_invocation_to_tiered_manager`):
///   the method is not offered, so it spends no compile retry;
/// * the by-name compile (`jit_bridge::try_jit_compile_callee_slow`, which
///   the background tasks, the eager callee chain and the callee compiler
///   share): no body of it is built, so no direct bind finds one;
/// * the inline resolver (`jit_bridge::resolve_inline_site_from`, both
///   tiers): no splice of it.
///
/// The doors that enter a method already refuse it
/// (`jit_bridge::jvmti_requires_interpreter_for`). Compile time and stride
/// only; `false` in one load while no breakpoint is set, and always `false`
/// in a build without the debugger.
///
/// Wave 44 (lane L1): also a method whose own bytecode invokes a method a
/// breakpoint sits in ([`calls_a_breakpoint_method`],
/// [`BREAKPOINT_CALLERS_STAY_INTERPRETED_ENABLED`]), so the frame one below
/// the stop is an interpreter frame whose locals `StackFrame.GetValues` /
/// `SetValues` serve, as HotSpot serves a compiled one.
#[cfg(feature = "experimental-debug")]
#[inline]
pub(crate) fn breakpoint_bars_compiling(
    shared: &SharedVm,
    declaring_class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> bool {
    let gates = &shared.debug.debugger_gates;
    // Interpreter round i4 wave 93 (lane L1, `CRATONVM_JDWP_SCOPED_METHOD_EVENTS`):
    // a method of a class a class-scoped method event request concerns is
    // not compiled, spliced or bound either while the request stands, so
    // every entry and exit of it runs the interpreter's suspend point. One
    // load for any other class.
    if gates.in_method_event_scope(declaring_class_id.as_u32()) {
        return true;
    }
    // Interpreter round i4 wave 95 (lane L1, `CRATONVM_JDWP_SCOPED_FIELD_WATCHES`):
    // likewise a method of a class whose constant pool names a watched field,
    // so every access of it runs the interpreter's field hook. One load for
    // any other class.
    if gates.in_field_watch_scope(declaring_class_id.as_u32()) {
        return true;
    }
    if !gates.any_breakpoint() {
        return false;
    }
    gates.holds_breakpoint(declaring_class_id.as_u32(), name, descriptor)
        || (BREAKPOINT_CALLERS_STAY_INTERPRETED_ENABLED
            && calls_a_breakpoint_method(shared, declaring_class_id, name, descriptor))
}

/// Switch for keeping a breakpoint's direct callers interpreted (interpreter
/// round i1 wave 44, lane L1; the stage of
/// `docs/internal/proposals/interpreter/i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands-20261007.md`
/// that needs no JIT change). HotSpot serves `StackFrame.GetValues` /
/// `SetValues` on a compiled frame by describing its locals at the call
/// (and deoptimizing it for a write); this VM's JIT records no description
/// of a compiled frame's locals at a call site, so a compiled caller of the
/// method a breakpoint stops in is listed with no locals (`OPAQUE_FRAME`).
/// `true`: a method whose bytecode invokes a method holding a breakpoint is
/// refused by the compile doors [`breakpoint_bars_compiling`] serves (no
/// body, no splice, no direct bind, not offered at a stride) while the
/// breakpoint stands, so the caller of every hit is an interpreter frame.
/// `false`: wave 43's behaviour (only the method the breakpoint sits in is
/// refused).
#[cfg(feature = "experimental-debug")]
pub(crate) const BREAKPOINT_CALLERS_STAY_INTERPRETED_ENABLED: bool = true;

/// Does the bytecode of the method `name` + `descriptor` declared by
/// `class_id` invoke (`invokevirtual`, `invokespecial`, `invokestatic`,
/// `invokeinterface`) a method whose name and descriptor are those of a
/// method holding a breakpoint (interpreter round i1 wave 44, lane L1)?
///
/// Matched by name and descriptor, whatever class the reference names: a
/// virtual call site names a supertype of the class the breakpoint sits in,
/// and refusing one compile too many costs only speed while a breakpoint
/// stands. A method of a JDK class is never kept interpreted for this
/// ([`jit_may_expand_a_method_of_without_a_record`]): the JDK's classes are
/// compiled without a `LocalVariableTable`, so a debugger shows no variables
/// of their frames anyway (JDI's `visibleVariables` throws
/// `AbsentInformationException`), and every `toString` caller of the JDK
/// would otherwise stay interpreted for a breakpoint in one `toString`.
///
/// Memoized per method and breakpoint generation
/// (`DebuggerGates::caller_verdict`); the scan takes the class manager's
/// read lock recursively (the by-name compile and the inline resolver hold
/// it when they ask). Positive control: `CRATONVM_DBG_JITC=1` prints
/// `[cratonvm-jitc] debugger keeps the caller of a breakpoint method
/// interpreted: <class>.<name><descriptor>` for each method the first time
/// it is judged so at a breakpoint generation.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
fn calls_a_breakpoint_method(
    shared: &SharedVm,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> bool {
    let gates = &shared.debug.debugger_gates;
    let method_id = crate::debug::jdwp_method_id(name, descriptor);
    let generation = gates.breakpoint_generation();
    if let Some(answer) = gates.caller_verdict(class_id.as_u32(), method_id, generation) {
        return answer;
    }
    let targets = gates.breakpoint_method_ids();
    let mut class_name = String::new();
    let answer = !targets.is_empty() && {
        let cm = shared.classes.class_manager.read_recursive();
        cm.class_store.get(class_id).is_some_and(|class| {
            if jit_may_expand_a_method_of_without_a_record(&class.name) {
                return false;
            }
            let Some(code) = class.find_method(name, descriptor).and_then(|m| m.code()) else {
                return false;
            };
            let calls = code_invokes_any(&class.constant_pool, &code.code[..], &targets);
            if calls {
                class_name.push_str(&class.name);
            }
            calls
        })
    };
    if gates.record_caller_verdict(class_id.as_u32(), method_id, generation, answer)
        && answer
        && crate::runtime::env_cache::dbg_jitc()
    {
        eprintln!(
            "[cratonvm-jitc] debugger keeps the caller of a breakpoint method interpreted: {class_name}.{name}{descriptor}"
        );
    }
    answer
}

/// Does `code` hold an invoke instruction (`invokevirtual`, `invokespecial`,
/// `invokestatic`, `invokeinterface`) whose method reference in `pool` has
/// the `jdwp_method_id` of one of `targets` (sorted)? Walked by instruction,
/// so an operand byte is never read as an opcode (wave 44, lane L1).
#[cfg(feature = "experimental-debug")]
fn code_invokes_any(
    pool: &cratonvm_reader::constant_pool::ConstantPool,
    code: &[u8],
    targets: &[u64],
) -> bool {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    let mut pc = 0usize;
    while let Some(&op) = code.get(pc) {
        if (0xb6..=0xb9).contains(&op) {
            if let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) {
                let nat = match pool.get(u16::from_be_bytes([hi, lo])) {
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
                if let Some((callee, callee_descriptor)) = nat {
                    let id = crate::debug::jdwp_method_id(callee, callee_descriptor);
                    if targets.binary_search(&id).is_ok() {
                        return true;
                    }
                }
            }
        }
        // `max(1)`: the walk terminates whatever the decoder answers.
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    false
}

/// [`breakpoint_bars_compiling`] in a build without the debugger.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(crate) fn breakpoint_bars_compiling(
    _shared: &SharedVm,
    _declaring_class_id: ClassId,
    _name: &str,
    _descriptor: &str,
) -> bool {
    false
}

/// May a compiled body run a method of the class `class_name` (internal
/// form) without any record the scoped withdrawal reads (interpreter round i1
/// wave 38, lane L1)? The JIT's call-site intrinsics (`cratonvm_jit::JitIntrinsic`:
/// `String.length` / `charAt` / `equals`, `StringBuilder.append`, `Math`,
/// the boxing methods, `ArrayList`, `Unsafe`, the FFM accessors, ...) expand
/// a JDK method at the call site from a fixed table, not from an inline plan,
/// so the body names it in neither `inlined_methods` nor `copied_classes`
/// (`try_resolve_*_intrinsic` in `jit/src/lib.rs`). Every such family is a
/// JDK class's, so a breakpoint in a JDK class takes the whole-cache
/// withdrawal; a program's own classes stay scoped.
///
/// Wave 40 (lane L1): also asked by `debug::publish_debugger_gates` for every
/// class holding a breakpoint. While one such class does, every method runs
/// interpreted and nothing is compiled ([`JDK_BREAKPOINT_INTERPRETER_ONLY_ENABLED`]),
/// because a caller compiled after the breakpoint could still expand the
/// method as a call-site intrinsic, which no compile door is asked about.
#[cfg(feature = "experimental-debug")]
pub(crate) fn jit_may_expand_a_method_of_without_a_record(class_name: &str) -> bool {
    ["java/", "javax/", "jdk/", "sun/", "com/sun/"]
        .iter()
        .any(|prefix| class_name.starts_with(prefix))
}

/// The cooperative slice the first loop-exit pause waits before its take-over
/// may freeze a peer still in compiled code. Every compiled loop polls at each
/// back edge and every compiled method at entry, so a slice of a few
/// milliseconds lets any loop whose iteration makes calls reach a poll.
const LOOP_EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(2);

/// Loop-exit pauses per request, at most (the slice doubles each time).
const LOOP_EXIT_ATTEMPTS: u32 = 3;

/// Take another loop-exit pause? Only when the last one froze a peer (which
/// therefore passed no poll during it) and an OSR body is still running: a
/// loop that left has counted out, and a pause that froze nobody reached
/// every poll there was.
fn loop_exit_pause_again(froze_a_peer: bool, osr_body_running: bool) -> bool {
    froze_a_peer && osr_body_running
}

// ---------------------------------------------------------------------------
// JDWP method events of native methods (interpreter round i1 wave 26, lane L1)
// ---------------------------------------------------------------------------
//
// HotSpot reports a `MethodEntry` and a `MethodExit` at location -1 for every
// native method a thread calls while a method event request is in force; the
// interpreter's suspend point (`deliver_breakpoint_if_set`), the only producer
// of method events until wave 26, never sees one: a native pushes no
// interpreter frame. Every native dispatch passes the native-call funnel
// (`vm_exec::safe_native_call_impl`), which reports them through the two
// functions below behind one relaxed load (`DebuggerGates::method_events_armed`)
// — stage 1 of the proposal
// interpreter-L1-proposal-native-method-events-at-the-native-funnel-FIXED-20260928.md.
//
// The native is named from the calling interpreter frame's invoke
// (`native_named_by_invoke`, confirmed by the callback the funnel runs), so a
// registered native standing in for a Java method (a `SyntheticStub` or
// `Bridge` of a method that is not `ACC_NATIVE`) reports nothing, as does a
// native called from compiled code (the top interpreter frame's invoke names
// the compiled method), a nested funnel call for another callback, and a
// native the interpreter answers without the funnel (`Thread.currentThread`
// from its mirror, below).
//
// Wave 27 (lane L1): the natives the interpreter's intrinsic table serves
// (`Object.getClass`, `Object.hashCode`, `System.arraycopy`) pass the funnel
// too, running the intrinsic's trampoline instead of the registered callback
// (`dispatch_static::invoke_cached_intrinsic`); [`callback_runs_method`]
// accepts either, so they are reported as HotSpot reports them. And a JVMTI
// env that listens for `MethodEntry` / `MethodExit` gets them as well (stage 4
// of the proposal), with the real `jmethodID` and, for an exceptional return,
// `was_popped_by_exception`: the funnel's gate carries a bit for it
// (`DebuggerGates::jvmti_method_events_armed`, raised by the C JVMTI table).
//
// Wave 28 (lane L1): a native called from compiled code is named from the
// innermost compiled activation's invoke (confirmed by the callback) and
// reported too; a JNI-bound native, which the general invocation path calls
// without the funnel, is reported by the JNI arms themselves
// ([`report_jni_native_method_entry`]); and a registered native or an
// intrinsic standing in for a Java method is not called while a debugger or
// an agent needs that method's frame — its bytecode runs instead
// ([`run_stood_in_java_method`]).
//
// Wave 29 (lane L1): a native reached through reflection or a method handle
// (`Method.invoke`, `MethodHandle.linkToStatic`) is called by the general
// invocation door (`vm_exec::invoke_on_class_shared_inner`), whose caller's
// invoke names the reflective or method-handle entry, never the native. That
// door holds the method, and names it to the funnel for the one call
// ([`with_held_native`]); the funnel uses the held name when the invoke names
// nothing the callback runs.

#[cfg(feature = "experimental-debug")]
thread_local! {
    /// The native the general invocation door is about to run through the
    /// funnel, while a debugger or an agent wants method events (wave 29):
    /// `(callback, declaring class id, JDWP method id)`, `(0, 0, 0)` for
    /// none. Set and restored by [`with_held_native`] around exactly one
    /// funnel call, and taken by the first [`report_native_method_entry`]
    /// that runs `callback`, so a nested call from inside the native never
    /// sees it. Per OS thread and per call, not per-VM state: it never
    /// outlives the call that set it.
    static HELD_NATIVE: std::cell::Cell<(usize, u32, u64)> =
        const { std::cell::Cell::new((0, 0, 0)) };
}

/// Run `call` — one native-call funnel call of `callback`, the registered
/// native of `name` + `descriptor` declared by `declaring` — with that method
/// named to the funnel's method-event report (interpreter round i1 wave 29,
/// lane L1; item 1 of
/// `docs/internal/fixed-bugs/interpreter-L1-jdwp-method-events-and-stop-miss-native-and-compiled-code-FIXED-20261005.md`).
/// The door that knows the method it calls (`vm_exec::invoke_on_class_shared_inner`,
/// every `NativeContext::invoke*` of reflection and method handles) calls it
/// behind `DebuggerGates::method_events_armed`; the report prefers the
/// method the caller's invoke names, so a direct call is reported as before.
/// Only an `ACC_NATIVE` method is held: a registered native standing in for a
/// Java method must not be reported as a native at location -1 (its events
/// are the bytecode's, [`run_stood_in_java_method`]).
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn with_held_native<R>(
    shared: &SharedVm,
    declaring: ClassId,
    name: &str,
    descriptor: &str,
    callback: usize,
    call: impl FnOnce() -> R,
) -> R {
    let acc_native = shared
        .classes
        .class_manager
        .read_recursive()
        .get_class(declaring)
        .and_then(|class| class.find_method(name, descriptor))
        .is_some_and(|method| method.is_native());
    if !acc_native {
        return call();
    }
    let held = (
        callback,
        declaring.as_u32(),
        crate::debug::jdwp_method_id(name, descriptor),
    );
    let prior = HELD_NATIVE.with(|slot| slot.replace(held));
    let out = call();
    HELD_NATIVE.with(|slot| slot.set(prior));
    out
}

/// The method [`with_held_native`] named for `callback`, taken (cleared), or
/// `None`: `(class id, JDWP method id)` as [`report_named_native_method_entry`]
/// takes them. Any held entry for this callback is cleared whether or not the
/// report uses it, so a nested call of the same native never inherits it.
#[cfg(feature = "experimental-debug")]
fn take_held_native(callback: usize) -> Option<(u64, u64)> {
    HELD_NATIVE.with(|slot| {
        let (held_callback, class_id, method_id) = slot.get();
        (held_callback != 0 && held_callback == callback).then(|| {
            slot.set((0, 0, 0));
            (u64::from(class_id), method_id)
        })
    })
}

/// Does the native-call funnel's `running_native` run the method `name` +
/// `descriptor` of the class named `class_name`? Its registered callback does,
/// and so (interpreter round i1 wave 27, lane L1) does the trampoline of the
/// interpreter intrinsic that serves that method from an `Intrinsic` inline
/// cache entry (`intrinsics::callback_for` of `intrinsics::lookup`'s kind):
/// `Object.getClass`, `Object.hashCode` and `System.arraycopy` reach the
/// funnel through `dispatch_static::invoke_cached_intrinsic` with that
/// trampoline, never with their registered callbacks, so until wave 27 no
/// method event and no blocked thread's native frame was named for them.
/// `false` for no running native (0). A class-manager-free lookup: the
/// caller names the class, and the intrinsic table is keyed by the JDK's own
/// names, which only the boot loader defines.
#[cfg(feature = "experimental-debug")]
pub(super) fn callback_runs_method(
    shared: &SharedVm,
    class_name: &str,
    name: &str,
    descriptor: &str,
    running_native: usize,
) -> bool {
    if running_native == 0 {
        return false;
    }
    shared
        .natives
        .native_methods
        .find(class_name, name, descriptor)
        .is_some_and(|callback| callback as usize == running_native) // Cast: fn address
        || cratonvm_native_builtins::intrinsics::lookup(class_name, name, descriptor).is_some_and(
            |kind| {
                // Cast: fn address
                cratonvm_native_builtins::intrinsics::callback_for(kind) as usize == running_native
            },
        )
}

/// Is `name` + `descriptor` of `class_name` a native method HotSpot's
/// interpreter serves from an intrinsic entry that posts no method event, so
/// that neither a JDWP request nor a JVMTI agent sees its entry or exit? Only
/// `Thread.currentThread` (`AbstractInterpreter::java_lang_Thread_currentThread`):
/// measured with a JDI `MethodEntryRequest` on HotSpot 25.0.3
/// (`tools/probes/interp/L1/L1W27JdiIntrinsicMethodEvents.java`), which
/// reports `getClass`, `hashCode`, `identityHashCode` and `arraycopy` but not
/// `currentThread`. CratonVM's interpreter answers a linked call site of it
/// from the thread's mirror without the funnel; this keeps the first call of
/// a site (the funnel, through the registered native) as silent.
#[cfg(feature = "experimental-debug")]
pub(super) fn hotspot_posts_no_method_events(
    class_name: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Thread"
        && name == "currentThread"
        && descriptor == "()Ljava/lang/Thread;"
}

/// Is `name` + `descriptor` of `class_name` a Java method HotSpot's template
/// interpreter serves from one of its math entries
/// (`AbstractInterpreter::java_lang_math_*`: `sin`, `cos`, `tan`, `tanh`,
/// `cbrt`, `abs(double)`, `sqrt`, `log`, `log10`, `exp`, `pow`, `fma`), which
/// run no bytecode and post no method event even while an agent or a
/// debugger needs the interpreter? Measured on HotSpot 25.0.3 for
/// `Math.abs(double)` and `Math.sqrt` (`L1W27JdiStandInMethodEvents`); the
/// rest are the same entry kind. [`run_stood_in_java_method`] leaves such a
/// method to the native that stands in for it (interpreter round i1 wave 28,
/// lane L1).
#[cfg(feature = "experimental-debug")]
pub(super) fn hotspot_serves_from_a_math_entry(
    class_name: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Math"
        && match descriptor {
            "(D)D" => matches!(
                name,
                "sin" | "cos" | "tan" | "tanh" | "cbrt" | "abs" | "sqrt" | "log" | "log10" | "exp"
            ),
            "(DD)D" => name == "pow",
            "(DDD)D" | "(FFF)F" => name == "fma",
            _ => false,
        }
}

/// Is `name` + `descriptor` of `class_name` `Float.float16ToFloat(S)F` or
/// `Float.floatToFloat16(F)S`, which HotSpot 25's template interpreter serves
/// from its `java_lang_Float_float16ToFloat` / `_floatToFloat16` entries (on
/// a CPU with F16C or AVX-512 VL) as it serves the math ones: no bytecode
/// runs and no method event is posted while a debugger asks for them
/// (measured on HotSpot 25.0.3, Windows x64, default flags and `-Xint`:
/// `tools/probes/interp/L1a/L1aW130JdiFloat16ShadowEvents.java`; interpreter
/// round i6 wave 130, lane L1a, i40-L1). Both are registered reviewed
/// `Intrinsic`s here, so the frame-only report
/// (`interpreter::shadow_named_by_invoke`) must not post for them.
/// [`run_stood_in_java_method`] needs no such check: their bodies read a
/// static (`$assertionsDisabled`), so `stood_in_bytecode_may_run` refuses
/// them already.
#[cfg(feature = "experimental-debug")]
pub(super) fn hotspot_serves_from_a_float16_entry(
    class_name: &str,
    name: &str,
    descriptor: &str,
) -> bool {
    class_name == "java/lang/Float"
        && matches!(
            (name, descriptor),
            ("float16ToFloat", "(S)F") | ("floatToFloat16", "(F)S")
        )
}

/// May [`run_stood_in_java_method`] run the bytecode of `name` +
/// `descriptor` (symbolic owner `owner_name`, declaring class `class_name`)
/// in place of the native standing in for it (wave 28 follow-up, lane L1b)?
///
/// * **Yes for the interpreter's intrinsic table** (`intrinsics::lookup`
///   names the triple: `String.length` / `charAt` / `isEmpty`, the
///   `StringBuilder` and `Integer` / `Long` entries, `Math.abs` / `min` /
///   `max` / `sqrt`, `Thread.onSpinWait`): the table's handlers reproduce the
///   Java method, so its bytecode computes the same answer (measured:
///   `L1W27JdiStandInMethodEvents` in all four modes), whether the call came
///   through the table's trampoline or through the registered native (the
///   `Math` rows are also registered `NativeKind::Intrinsic`).
/// * **No for any other reviewed `NativeKind::Intrinsic` shadow over
///   bytecode**: such a native wins over real bytecode BECAUSE that bytecode
///   is wrong in this VM. `Class.getName` is the measured case: its bytecode
///   reads `Class.name`, which here is an overlay holding the INTERNAL name,
///   so running it under a breakpoint or a step answered `java/lang/...`
///   (`L1W25JdiStopMonitors` under `--jdk-only`, host run of wave 28;
///   `native-builtins/src/lib.rs`, the `getName` registration). Excluded with
///   it: every Intrinsic-kind registration of a method with bytecode outside
///   the table (the other `Class` / `String` / `Math` shadows, e.g.
///   `Math.min(FF)`, `Math.max(DD)`).
/// * **Bridges and synthetic stubs: under `--jdk-only` only.** There a
///   non-Intrinsic registration never wins over bytecode by policy, so one
///   the funnel runs over bytecode is a dial or door exception whose
///   bytecode is authoritative; under `--compatible` such a native routinely
///   stands in for bytecode that does not run in this VM, and that mode's
///   dispatch stays as it was.
/// * **Yes, whatever the registration and the mode, for bytecode that reads
///   nothing but its arguments** (`self_contained`,
///   [`bytecode_reads_only_its_arguments`]; interpreter round i2 wave 62,
///   lane L1): such bytecode cannot lean on VM state this VM keeps
///   elsewhere, which is the only reason a stand-in is kept over it, so it
///   computes what the native computes. `Integer.numberOfLeadingZeros`, a
///   reviewed `Intrinsic`, is the measured case: HotSpot's agent steps
///   through its bytecode, and this VM ran the native
///   (`tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.java`).
#[cfg(feature = "experimental-debug")]
pub(super) fn stood_in_bytecode_may_run(
    shared: &SharedVm,
    owner_name: &str,
    class_name: &str,
    name: &str,
    descriptor: &str,
    self_contained: bool,
) -> bool {
    use cratonvm_native_builtins::intrinsics;
    // Never the reflection and method-handle doors (`Method.invoke`,
    // `Constructor.newInstance`, `MethodHandle` / `Lookup` members): their
    // natives ARE the dispatch, and their JDK bytecode leans on VM state this
    // VM answers through natives of its own (`AccessibleObject.checkAccess`
    // -> `Module.isExported`, `MemberName`). Running `Method.invoke`'s
    // bytecode under a method-event request threw `IllegalAccessException`
    // for a public `java.lang.Float` method and ended the debuggee
    // (interpreter round i1 wave 29 host run,
    // `L1/L1W29JdiReflectedNativeMethodEvents`, `--jdk-only`).
    const DOOR_PACKAGES: [&str; 3] = [
        "java/lang/reflect/",
        "java/lang/invoke/",
        "jdk/internal/reflect/",
    ];
    if DOOR_PACKAGES
        .iter()
        .any(|p| owner_name.starts_with(p) || class_name.starts_with(p))
    {
        return false;
    }
    if intrinsics::lookup(owner_name, name, descriptor).is_some()
        || intrinsics::lookup(class_name, name, descriptor).is_some()
    {
        return true;
    }
    // Interpreter round i1 wave 46 (lane L1): never `java.lang.Thread`'s own
    // methods (the blocking `sleep` family runs through
    // [`run_blocking_standin`] instead). Their JDK bytecode reads the thread
    // state HotSpot keeps in `Thread.eetop` and `Thread$FieldHolder.threadStatus`,
    // which this VM never writes: it keeps a thread's state in its thread
    // registry, and the registered natives read it there. Running
    // `Thread.isAlive`, `getState` and `join` as bytecode under a method
    // event request answered `false`, `NEW` and "at once" for a thread that
    // had started and was running, so a `t.start(); t.join()` did not wait
    // (`L1W43JdiForceEarlyReturn`'s `free = false`, `other=NEW
    // aliveAfterStart=false joinMs=0 ... took=true`, host run of wave 46).
    // HotSpot also posts those methods' `MethodEntry` / `MethodExit`; this
    // VM does not (a known difference, never a wrong value).
    const THREAD_STATE_CLASSES: [&str; 2] = ["java/lang/Thread", "java/lang/Thread$FieldHolder"];
    if THREAD_STATE_CLASSES
        .iter()
        .any(|c| owner_name == *c || class_name == *c)
    {
        return false;
    }
    if self_contained {
        return true;
    }
    let registry = &shared.natives.native_methods;
    let kind = registry
        .find_with_kind(owner_name, name, descriptor)
        .or_else(|| registry.find_with_kind(class_name, name, descriptor))
        .map(|(_, kind)| kind);
    match kind {
        Some(cratonvm_native_api::NativeKind::Intrinsic) => false,
        _ => shared.compatibility_mode().is_jdk_only(),
    }
}

/// How many `invokestatic` levels [`bytecode_reads_only_its_arguments`]
/// follows (`Long.numberOfLeadingZeros` calls `Integer`'s: two levels).
#[cfg(feature = "experimental-debug")]
const SELF_CONTAINED_CALL_DEPTH: u8 = 3;

/// Does method `index` of class `declaring` run bytecode that reads nothing
/// but its arguments (interpreter round i2 wave 62, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-single-step-skips-a-method-a-registered-native-answers-FIXED-20261026.md`)?
/// No field access, no allocation, no `ldc` / `ldc_w` but of an `int` or a
/// `float` (a class, string, method-type or dynamic constant resolves through
/// VM state), no `checkcast` /
/// `instanceof`, no monitor, no `invokedynamic`, and no call but an
/// `invokestatic` of a non-native method that passes this test itself
/// (`depth` more levels). Arithmetic, locals, branches, switches, array
/// access to an argument and `athrow` of one are allowed. Such bytecode
/// computes exactly what a native standing in for it computes, in any mode
/// ([`stood_in_bytecode_may_run`]). The pure `Integer` / `Long` bit methods
/// pass; `Class.getName` (a field read) and anything that calls a virtual
/// method do not. Cold: asked only when the funnel is about to run a stand-in
/// while an agent or a debugger needs a frame.
#[cfg(feature = "experimental-debug")]
pub(super) fn bytecode_reads_only_its_arguments(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    declaring: ClassId,
    index: u32,
    depth: u8,
) -> bool {
    const LDC: u8 = 0x12;
    const LDC_W: u8 = 0x13;
    const INVOKESTATIC: u8 = 0xb8;
    let Some(class) = cm.get_class(declaring) else {
        return false;
    };
    let Some(method) = usize::try_from(index)
        .ok()
        .and_then(|i| class.methods.get(i))
    else {
        return false;
    };
    if method.is_native() {
        return false;
    }
    let Some(code) = method.code().map(|c| &c.code[..]) else {
        return false;
    };
    let mut pc = 0usize;
    while pc < code.len() {
        let op = code[pc];
        match op {
            // ldc, ldc_w: only an `int` or a `float` constant
            // (`Integer.numberOfLeadingZeros` loads 65536 with `ldc_w`).
            LDC | LDC_W => {
                let index = if op == LDC {
                    code.get(pc + 1).map(|&b| u16::from(b))
                } else {
                    match (code.get(pc + 1), code.get(pc + 2)) {
                        (Some(&hi), Some(&lo)) => Some(u16::from_be_bytes([hi, lo])),
                        _ => None,
                    }
                };
                let numeric = index
                    .and_then(|i| class.constant_pool.get(i))
                    .is_some_and(|e| {
                        matches!(
                            e,
                            ConstantPoolEntry::Integer(_) | ConstantPoolEntry::Float(_)
                        )
                    });
                if !numeric {
                    return false;
                }
            }
            // getstatic..putfield; invokevirtual, invokespecial,
            // invokeinterface, invokedynamic; new, newarray, anewarray;
            // checkcast, instanceof; monitorenter, monitorexit;
            // multianewarray.
            0xb2..=0xb7 | 0xb9..=0xbd | 0xc0..=0xc3 | 0xc5 => return false,
            INVOKESTATIC => {
                if depth == 0 {
                    return false;
                }
                let (Some(&hi), Some(&lo)) = (code.get(pc + 1), code.get(pc + 2)) else {
                    return false;
                };
                let Some((callee, callee_index)) =
                    static_callee(shared, cm, class, declaring, u16::from_be_bytes([hi, lo]))
                else {
                    return false;
                };
                if !bytecode_reads_only_its_arguments(shared, cm, callee, callee_index, depth - 1) {
                    return false;
                }
            }
            _ => {}
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc);
    }
    true
}

/// The method the `invokestatic` at constant-pool index `cp_index` of
/// `class` (id `class_id`) names, resolved here: `(declaring class, method
/// index)`, `None` when it does not resolve without loading anything.
#[cfg(feature = "experimental-debug")]
fn static_callee(
    shared: &SharedVm,
    cm: &crate::classloading::ClassManager,
    class: &crate::classloading::Class,
    class_id: ClassId,
    cp_index: u16,
) -> Option<(ClassId, u32)> {
    let (class_index, nat_index) = match class.constant_pool.get(cp_index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        }
        | ConstantPoolEntry::InterfaceMethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner_name = class.constant_pool.get_class_name(class_index)?;
    let (name, descriptor) = class.constant_pool.get_name_and_type(nat_index)?;
    let owner = cm.find_cp_class_for_class(owner_name, class_id)?;
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let found = resolver
        .declared_method(cm, resolver.scope(owner), name, descriptor)
        .ok()?;
    resolver.adopt(found).ok()
}

/// The native-call funnel is about to run `callback` in place of a JAVA
/// method — one with bytecode, not `ACC_NATIVE` — that a debugger or an agent
/// needs to see run in a frame: run that method's bytecode instead, through
/// [`super::execute`], and answer its result, which the funnel returns in
/// place of the callback's (interpreter round i1 wave 28, lane L1; stage 1 of
/// `docs/internal/fixed-bugs/interpreter-L1-methods-served-without-a-frame-report-no-method-events-FIXED-20261004.md`).
///
/// Two routes answer a call of such a method without a frame: an `Intrinsic`
/// inline-cache entry (`dispatch_static::invoke_cached_intrinsic`, both the
/// static and the virtual arm) and a registered native standing in for it
/// (a `Native` / `VirtualNative` entry, or the slow path's native-first
/// choice). Every one of them ends in the funnel with the intrinsic's
/// trampoline or the registered callback, so this one hook covers them all
/// without a test on any arm: it runs behind the funnel's existing
/// `DebuggerGates::method_events_armed` load, whose bits are up only while a
/// JDWP method event request, a JVMTI method event listener, or (wave 28) a
/// breakpoint or a step request is in force.
///
/// The method runs in a frame when a JDWP method event request is in force,
/// when a JVMTI env listens for method events or (wave 29) single-steps, or
/// when a breakpoint sits in it or a step may enter it
/// (`DebuggerGates::concerns_method`): its
/// `MethodEntry` / `MethodExit` are posted from that frame at its first and
/// its returning bytecode index, a breakpoint in it stops the thread, and a
/// step into the call steps into it, as HotSpot does. `None`, and the callback
/// runs, for anything else:
///
/// * a callback the top interpreter frame's invoke does not name (a nested
///   call from inside a native), or a call made by a compiled activation above
///   that frame;
/// * an `ACC_NATIVE` method (the funnel reports its events itself,
///   [`report_native_method_entry`]) or one without bytecode (a `--compatible`
///   synthetic stub);
/// * a virtual or interface call whose callback does not stand in for the
///   method the receiver's class selects (wave 29: the selection is made
///   here, so `CharSequence.length` on a `String` runs `String.length`; until
///   then only a target no override could replace ran);
/// * a method HotSpot also serves without a frame
///   ([`hotspot_serves_from_a_math_entry`]);
/// * a method whose bytecode this VM must not run in place of its native
///   ([`stood_in_bytecode_may_run`]: a reviewed `NativeKind::Intrinsic`
///   shadow outside the interpreter's intrinsic table, such as
///   `Class.getName`; any other stand-in under `--compatible`);
/// * work the debugger runs on a parked thread, for the JDWP requests
///   (`debug::debugger_hooks_suppressed`).
///
/// Called with the arguments pinned and the thread still a mutator, before
/// the native thread state is entered; [`super::execute`] roots the arguments
/// itself from then on.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn run_stood_in_java_method(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: usize,
    args: &[Value],
) -> Option<MethodCallResult> {
    const INVOKEVIRTUAL: u8 = 0xb6;
    const INVOKESPECIAL: u8 = 0xb7;
    const INVOKESTATIC: u8 = 0xb8;
    const INVOKEINTERFACE: u8 = 0xb9;
    let gates = &shared.debug.debugger_gates;
    let suppressed = crate::debug::debugger_hooks_suppressed();
    let jdwp_events = gates.jdwp_method_events_armed() && !suppressed;
    let jdwp_frames = gates.java_frames_armed() && !suppressed;
    let jvmti_events = gates.jvmti_method_events_armed();
    // Wave 29: an agent single-stepping (a JVMTI `SingleStep` listener) steps
    // into the method, as HotSpot's interpreter-only mode does.
    let jvmti_frames = gates.jvmti_frames_armed();
    // Wave 46 (lane L1): while a debugger is attached, a Java method that
    // blocks the thread and that a registered native stands in for
    // (`Thread.sleep`, `Object.wait`: `debug::BLOCKING_STANDINS`) runs its
    // bytecode, so the thread blocks in the genuine native leaf
    // (`sleepNanos0`, `wait0`) under the JDK frames HotSpot lists, and a
    // suspension there parks at that native's return
    // (`park_if_suspended_at_native_exit`). A few loads for any other native.
    if gates.blocking_standins_armed() && !suppressed {
        let matches = gates.blocking_standin_matches(callback);
        if matches != 0 {
            if let Some(out) = run_blocking_standin(shared, thread, matches, args) {
                return Some(out);
            }
        }
    }
    if !jdwp_events && !jdwp_frames && !jvmti_events && !jvmti_frames {
        return None;
    }
    let (caller_id, opcode, cp_index) = {
        let top = thread.frames.last()?;
        let pc = top.last_instr_pc;
        let opcode = *top.code.get(pc)?;
        if !matches!(
            opcode,
            INVOKEVIRTUAL | INVOKESPECIAL | INVOKESTATIC | INVOKEINTERFACE
        ) {
            return None;
        }
        let cp_index = u16::from_be_bytes([*top.code.get(pc + 1)?, *top.code.get(pc + 2)?]);
        (top.class_id, opcode, cp_index)
    };
    // The cheap filter first (the invoke's symbolic reference, then a
    // registry probe): does the callback run the method the invoke names?
    // `read_recursive` (both reads): a native may run while a caller up this
    // thread's stack holds a read of the class manager, and a plain read
    // queues behind a waiting writer (see `invoke::resolve_method_metadata`).
    let (owner_name, name, descriptor): (Arc<str>, Arc<str>, Arc<str>) = {
        let cm = shared.classes.class_manager.read_recursive();
        let caller = cm.get_class(caller_id)?;
        let (class_index, nat_index) = match caller.constant_pool.get(cp_index)? {
            ConstantPoolEntry::MethodReference {
                class_index,
                name_and_type_index,
            }
            | ConstantPoolEntry::InterfaceMethodReference {
                class_index,
                name_and_type_index,
            } => (*class_index, *name_and_type_index),
            _ => return None,
        };
        let owner_name = caller.constant_pool.get_class_name(class_index)?;
        let (name, descriptor) = caller.constant_pool.get_name_and_type(nat_index)?;
        (
            Arc::from(owner_name),
            Arc::from(name),
            Arc::from(descriptor),
        )
    };
    // Wave 29: a virtual or interface call is also examined when the callback
    // does not run the symbolic owner's method: the receiver's class selects
    // the method (`CharSequence.length` on a `String`, `Number.intValue` on an
    // `Integer`), and the callback is matched against that below.
    let owner_runs = callback_runs_method(shared, &owner_name, &name, &descriptor, callback);
    let virtual_site = matches!(opcode, INVOKEVIRTUAL | INVOKEINTERFACE);
    if !owner_runs && !virtual_site {
        return None;
    }
    // A compiled activation above the top interpreter frame made this call.
    let compiled = super::capture_blocked_compiled_view(&thread.frames);
    if compiled
        .rows
        .last()
        .is_some_and(|row| row.below as usize >= thread.frames.len())
    // Widening: a frame index
    {
        return None;
    }
    let (declaring, class_name, receiver_name, self_contained) = {
        let cm = shared.classes.class_manager.read_recursive();
        let owner = cm.find_cp_class_for_class(&owner_name, caller_id)?;
        let resolver = crate::runtime::resolve::MemberResolver::new(shared);
        // The method the symbolic reference resolves to (`None` when it does
        // not resolve here: then only a receiver selection can name one).
        let resolved = resolver
            .declared_method(&cm, resolver.scope(owner), &name, &descriptor)
            .ok()
            .and_then(|found| resolver.adopt(found).ok());
        let mut receiver_name: Option<Arc<str>> = None;
        let (declaring, index) = if virtual_site {
            // Wave 29 (item 2 of
            // `i27-L1-methods-served-without-a-frame-report-no-method-events`):
            // a virtual call an override could answer runs the method the
            // RECEIVER's class selects (JVMS 5.4.6, the dispatch the
            // interpreter's doors make), provided the callback stands in for
            // exactly that method: it is the receiver class's or the
            // selected declaring class's registration or intrinsic, or the
            // symbolic owner's when no override intervenes. Anything else
            // (an override the native does not stand in for) keeps the native.
            let Some(Value::Object(Some(receiver))) = args.first() else {
                return None;
            };
            let receiver_id = shared.mem.heap.class_id_of(*receiver);
            let (_, selected) = crate::runtime::resolve::selection::select_for_receiver_dispatch(
                cm.class_store(),
                receiver_id,
                Some(owner),
                &name,
                &descriptor,
            )?;
            let selected_class = cm.get_class(selected)?;
            let receiver_class_name = Arc::clone(&cm.get_class(receiver_id)?.name);
            let stands_in =
                callback_runs_method(shared, &selected_class.name, &name, &descriptor, callback)
                    || callback_runs_method(
                        shared,
                        &receiver_class_name,
                        &name,
                        &descriptor,
                        callback,
                    )
                    || (owner_runs && resolved.is_some_and(|(id, _)| id == selected));
            receiver_name = Some(receiver_class_name);
            if !stands_in {
                return None;
            }
            let index = selected_class
                .methods
                .iter()
                .position(|m| *m.name == *name && *m.descriptor == *descriptor)?;
            (selected, u32::try_from(index).ok()?)
        } else {
            resolved?
        };
        let class = cm.get_class(declaring)?;
        let method = class.methods.get(usize::try_from(index).ok()?)?;
        if method.is_native() || method.code().is_none() {
            return None;
        }
        // Every target here is fixed: a static or special invoke names it,
        // and a virtual one was selected for the receiver above. (Until wave
        // 29 a virtual call ran only when no override could exist: a static,
        // private or final method, a final class or a final symbolic owner.)
        // Wave 62 (lane L1): whether its bytecode reads only its arguments.
        let self_contained = bytecode_reads_only_its_arguments(
            shared,
            &cm,
            declaring,
            index,
            SELF_CONTAINED_CALL_DEPTH,
        );
        (
            declaring,
            Arc::clone(&class.name),
            receiver_name,
            self_contained,
        )
    };
    if hotspot_serves_from_a_math_entry(&class_name, &name, &descriptor) {
        return None;
    }
    if !stood_in_bytecode_may_run(
        shared,
        &owner_name,
        &class_name,
        &name,
        &descriptor,
        self_contained,
    ) {
        return None;
    }
    // Wave 29: a receiver-selected method must also pass under the name the
    // receiver's dispatch looked up (its registration may be a reviewed
    // `Intrinsic` shadow where the symbolic owner, an interface, has none).
    if receiver_name.is_some_and(|receiver_name| {
        !stood_in_bytecode_may_run(
            shared,
            &receiver_name,
            &class_name,
            &name,
            &descriptor,
            self_contained,
        )
    }) {
        return None;
    }
    let needs_frame = jdwp_events
        || jvmti_events
        || jvmti_frames
        || (jdwp_frames && gates.concerns_method(declaring.as_u32(), &name, &descriptor));
    if !needs_frame {
        return None;
    }
    // Positive control (wave 29): `CRATONVM_FRAME_TRACE=1` names every
    // stood-in method the funnel runs as bytecode, and the symbolic owner the
    // invoke named (a supertype for a receiver-selected method).
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[STOOD_IN] {}.{}{} owner={} jvmti_step={}",
            class_name, name, descriptor, owner_name, jvmti_frames
        );
    }
    Some(super::execute(
        shared,
        thread,
        declaring,
        &name,
        &descriptor,
        args,
    ))
}

/// The native leaf of the blocking stand-in (`debug::BLOCKING_STANDINS`) the
/// invoke at `pc` of `code` (a method of class `caller_id`) calls, as `(class
/// id, JDWP method id)`: `Thread.sleepNanos0` (JDK 25; `sleep0`, JDK 21) for
/// `Thread.sleep`, `Object.wait0` for `Object.wait` (interpreter round i1
/// wave 46, lane L1). HotSpot lists that native on top of a thread blocked
/// in the method, where this VM's registered stand-in runs no frame of its
/// own; `native_named_by_invoke` names no native for a method with
/// bytecode. With `running_native` (the callback recorded while a debugger
/// is attached, or the one a native-exit park returns from) the stand-in
/// must be the method that callback runs; without one (a thread that
/// blocked before the debugger attached) the top frame's invoke is the call
/// in progress, as for a fixed native target. `None` for any other call.
/// Only the leaf: since wave 47 [`blocking_standin_head`] lists the JDK's
/// Java levels too, and falls back to this when their chain cannot be read.
#[cfg(feature = "experimental-debug")]
fn blocking_standin_leaf(
    shared: &SharedVm,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    running_native: Option<usize>,
) -> Option<(u64, u64)> {
    use cratonvm_reader::constant_pool::ConstantPoolEntry;
    const INVOKEVIRTUAL: u8 = 0xb6;
    const INVOKESPECIAL: u8 = 0xb7;
    const INVOKESTATIC: u8 = 0xb8;
    if !matches!(
        code.get(pc).copied(),
        Some(INVOKEVIRTUAL | INVOKESPECIAL | INVOKESTATIC)
    ) {
        return None;
    }
    let cp_index = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
    let cm = shared.classes.class_manager.read_recursive();
    let caller = cm.get_class(caller_id)?;
    let (class_index, nat_index) = match caller.constant_pool.get(cp_index)? {
        ConstantPoolEntry::MethodReference {
            class_index,
            name_and_type_index,
        } => (*class_index, *name_and_type_index),
        _ => return None,
    };
    let owner_name = caller.constant_pool.get_class_name(class_index)?;
    let (name, descriptor) = caller.constant_pool.get_name_and_type(nat_index)?;
    let owner = cm.find_cp_class_for_class(owner_name, caller_id)?;
    let resolver = crate::runtime::resolve::MemberResolver::new(shared);
    let found = resolver
        .declared_method(&cm, resolver.scope(owner), name, descriptor)
        .ok()?;
    let (declaring, _) = resolver.adopt(found).ok()?;
    let class = cm.get_class(declaring)?;
    let (standin_class, standin_name, standin_descriptor) = crate::debug::BLOCKING_STANDINS
        .iter()
        .copied()
        .find(|&(c, n, d)| c == &*class.name && n == name && d == descriptor)?;
    // Both census methods are static or final: no override can answer.
    let leaves: &[&str] = if standin_name == "sleep" {
        &["sleepNanos0", "sleep0"]
    } else {
        &["wait0"]
    };
    let leaf = leaves.iter().copied().find(|leaf| {
        class
            .find_method(leaf, "(J)V")
            .is_some_and(|m| m.is_native())
    })?;
    let class_id = u64::from(declaring.as_u32());
    drop(cm);
    if let Some(callback) = running_native {
        if !callback_runs_method(
            shared,
            standin_class,
            standin_name,
            standin_descriptor,
            callback,
        ) {
            return None;
        }
    }
    Some((class_id, crate::debug::jdwp_method_id(leaf, "(J)V")))
}

/// The head of a debugger's listing of a thread blocked in, or suspended at
/// the return of, a registered native standing in for the JDK method the
/// invoke at `pc` of `code` (a method of class `caller_id`) calls
/// (interpreter round i1 wave 47, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-a-sleep-or-wait-stand-in-lists-only-its-native-leaf-FIXED-20261011.md`):
/// the method's native leaf and the Java levels between it and the call, as
/// HotSpot 25.0.3 lists them -- `Thread.sleepNanos0` (native),
/// `Thread.sleepNanos`, `Thread.sleep`; `Object.wait0`, `Object.wait(long)`;
/// `Unsafe.park`, `LockSupport.parkNanos` -- read from the real class bytes
/// (`stackwalker::debugger_standin_frames`). The stand-in ran none of them,
/// so the levels have no locals ([`super::NativeHead`]). Both modes: a
/// `--compatible` thread always blocks in the stand-in; a `--jdk-only` one
/// when it blocked before the debugger attached (after, the bytecode runs,
/// [`run_blocking_standin`]).
///
/// With `running_native` (the callback recorded while a debugger is
/// attached, or the one a native-exit park returns from) the stand-in must be
/// the method that callback runs; without one (a thread that blocked before
/// the debugger attached) the top frame's invoke is the call in progress.
/// When the chain cannot be read, the wave-46 answer: the leaf alone for a
/// `debug::BLOCKING_STANDINS` method ([`blocking_standin_leaf`]). `None` for
/// any other call. Takes the class-manager lock: call it before the
/// debug-state lock. Positive control: `CRATONVM_FRAME_TRACE=1` prints
/// `[STANDIN_HEAD] <class>.<name><descriptor> levels=<n>` per head built
/// with its Java levels.
#[cfg(feature = "experimental-debug")]
pub(super) fn blocking_standin_head(
    shared: &SharedVm,
    caller_id: ClassId,
    code: &[u8],
    pc: usize,
    running_native: Option<usize>,
) -> Option<super::NativeHead> {
    let chain = {
        let cm = shared.classes.class_manager.read_recursive();
        crate::runtime::stackwalker::debugger_standin_frames(
            &cm.class_store,
            caller_id,
            code,
            pc,
            &mut |requester: ClassId, name: &str| cm.find_class_by_name_for_class(name, requester),
        )
    };
    let head = chain.as_deref().and_then(|chain| {
        let entry = chain.first()?;
        let descriptor = entry.method_descriptor.as_deref()?;
        if let Some(callback) = running_native {
            if !callback_runs_method(
                shared,
                &entry.class_name,
                &entry.method_name,
                descriptor,
                callback,
            ) {
                return None;
            }
        }
        let head = super::NativeHead::from_standin_chain(chain)?;
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[STANDIN_HEAD] {}.{}{} levels={}",
                entry.class_name,
                entry.method_name,
                descriptor,
                head.levels().len()
            );
        }
        Some(head)
    });
    head.or_else(|| {
        blocking_standin_leaf(shared, caller_id, code, pc, running_native)
            .map(|(class_id, method_id)| super::NativeHead::leaf(class_id, method_id))
    })
}

/// [`run_stood_in_java_method`] for a blocking stand-in (interpreter round i1
/// wave 46, lane L1; `debug::BLOCKING_STANDINS`): `matches` has bit `i` set
/// for each entry of that table whose registered callback is the one about
/// to run; the entry whose parameters `args` fill is run as bytecode, from
/// its declaring class, whoever made the call (an interpreted caller or a
/// compiled one: the callback names the method, no invoke is decoded). The
/// callback runs instead (`None`) under `--compatible` (unless
/// `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE` is on, wave 48), for the
/// body's own leaf call ([`leaf_call_of_stood_in_body`]), on a virtual thread, whose JDK `sleep` /
/// `wait` path parks through `VirtualThread` where this VM's natives release
/// the carrier themselves, and for a method without bytecode (JDK 17's
/// `native` `sleep(long)` / `wait(long)`, a `--compatible` stub).
/// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[STOOD_IN_BLOCKING]
/// <class>.<name><descriptor>` per such call.
#[cfg(feature = "experimental-debug")]
fn run_blocking_standin(
    shared: &SharedVm,
    thread: &mut JvmThread,
    matches: u8,
    args: &[Value],
) -> Option<MethodCallResult> {
    // `--compatible` keeps its stand-ins unless
    // `CRATONVM_DEBUG_COMPATIBLE_BLOCKING_BYTECODE` is on (wave 48, lane L1;
    // `DebuggerGates::compatible_blocking_bytecode`); a thread blocked in a
    // stand-in lists the JDK levels it skipped as opaque rows
    // ([`blocking_standin_head`]).
    if matches!(thread.kind, crate::threading::ThreadKind::Virtual)
        || !(shared.compatibility_mode().is_jdk_only()
            || shared.debug.debugger_gates.compatible_blocking_bytecode())
    {
        return None;
    }
    let (class_name, name, descriptor) = crate::debug::BLOCKING_STANDINS
        .iter()
        .enumerate()
        .filter(|&(i, _)| matches & (1u8 << i) != 0)
        .map(|(_, &entry)| entry)
        .find(|&(class_name, _, descriptor)| {
            // `Object.wait` takes its receiver; `Thread.sleep` is static.
            let receiver = usize::from(class_name == "java/lang/Object");
            descriptor_parameter_count(descriptor) + receiver == args.len()
        })?;
    // The JDK body's own leaf call. `--compatible` registers ONE callback for
    // `Object.wait(long)` and its native `wait0(long)`
    // (`native_object_wait_timeout`), so the leaf the body calls matches the
    // row it came from; running the body again would recurse without end.
    // A call made by the stood-in body's own frame to a method of another
    // name is that leaf (`wait()` -> `wait(long)` keeps the name, and runs
    // `wait(long)`'s body in turn, as on HotSpot).
    if leaf_call_of_stood_in_body(shared, thread, class_name, name) {
        return None;
    }
    let declaring = {
        let cm = shared.classes.class_manager.read_recursive();
        let id = cm.get_loaded_class_id(class_name)?;
        let class = cm.get_class(id)?;
        if class.origin.is_compatibility_stub() {
            return None;
        }
        let method = class.find_method(name, descriptor)?;
        if method.is_native() || method.code().is_none() {
            return None;
        }
        id
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[STOOD_IN_BLOCKING] {class_name}.{name}{descriptor}");
    }
    Some(super::execute(
        shared, thread, declaring, name, descriptor, args,
    ))
}

/// Is this native call the leaf a stood-in blocking body calls, rather than a
/// call of the body's method itself (wave 48, lane L1)? `true` when the top
/// interpreter frame is a `name` method of `class_name` (the body) standing
/// at an invoke of a method of ANOTHER name (`wait(long)` at its `wait0`).
/// Only the stood-in bodies' own frames answer `true`; any other caller,
/// interpreted or compiled, falls through.
#[cfg(feature = "experimental-debug")]
fn leaf_call_of_stood_in_body(
    shared: &SharedVm,
    thread: &JvmThread,
    class_name: &str,
    name: &str,
) -> bool {
    let Some(top) = thread.frames.last() else {
        return false;
    };
    if top.method_name() != name || top.class_name() != class_name {
        return false;
    }
    let pc = top.last_instr_pc;
    // invokevirtual / invokespecial / invokestatic / invokeinterface
    if !matches!(top.code.get(pc), Some(0xb6..=0xb9)) {
        return false;
    }
    let (Some(&hi), Some(&lo)) = (top.code.get(pc + 1), top.code.get(pc + 2)) else {
        return false;
    };
    let cp_index = u16::from_be_bytes([hi, lo]);
    // `read_recursive`: see `run_stood_in_java_method`.
    let cm = shared.classes.class_manager.read_recursive();
    let Some(class) = cm.get_class(top.class_id) else {
        return false;
    };
    let nat_index = match class.constant_pool.get(cp_index) {
        Some(
            ConstantPoolEntry::MethodReference {
                name_and_type_index,
                ..
            }
            | ConstantPoolEntry::InterfaceMethodReference {
                name_and_type_index,
                ..
            },
        ) => *name_and_type_index,
        _ => return false,
    };
    class
        .constant_pool
        .get_name_and_type(nat_index)
        .is_some_and(|(callee, _)| callee != name)
}

/// The number of parameters `descriptor` declares (a `long` or a `double`
/// is one, as the funnel's arguments carry it); 0 for a malformed one.
#[cfg(feature = "experimental-debug")]
fn descriptor_parameter_count(descriptor: &str) -> usize {
    let Some(params) = descriptor
        .strip_prefix('(')
        .and_then(|rest| rest.split(')').next())
    else {
        return 0;
    };
    let bytes = params.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        while bytes.get(i) == Some(&b'[') {
            i += 1;
        }
        if bytes.get(i) == Some(&b'L') {
            while i < bytes.len() && bytes[i] != b';' {
                i += 1;
            }
        }
        i += 1;
        count += 1;
    }
    count
}

/// A genuine native method the funnel reported the entry of — or would have,
/// had an entry request been in force — for an interpreted caller: what
/// [`report_native_method_exit`] needs to report its exit.
#[cfg(feature = "experimental-debug")]
pub(crate) struct DebuggedNative {
    /// Wire class id of the method's declaring class.
    class_id: u64,
    /// The method's JDWP method id.
    method_id: u64,
    /// The declaring class's internal name (`ClassMatch` / `ClassExclude`).
    class_name: std::sync::Arc<str>,
    /// The first byte of the method's return type (`V` for `void`).
    return_type: u8,
    /// The caller's invoke (`last_instr_pc` of the top interpreter frame):
    /// where the caller is listed while the thread is suspended here.
    caller_pc: usize,
    /// The `ClassOnly` ids the declaring class answers (its supertypes among
    /// the requests' filter classes).
    location_supers: Vec<u64>,
    /// JDWP method events were armed (and not suppressed) at the entry: the
    /// exit is matched against the JDWP requests.
    jdwp: bool,
    /// The method's JVMTI `jmethodID` (`(class_id << 32) | method_index`,
    /// `jvmti::resolve_method_id_for_vm`) when a JVMTI env of this VM listened
    /// for method events at the entry (wave 27): the exit is posted to it.
    jvmti_method_id: Option<u64>,
    /// The arguments may have moved before the callback runs: the entry's
    /// event set suspended the thread and it parked, or (wave 27) an agent's
    /// `MethodEntry` callback ran on this thread, either of which may have
    /// collected. The funnel re-reads the arguments from their pins.
    pub(crate) parked: bool,
    /// The code index the entry's events and listing carry:
    /// `NATIVE_FRAME_LOCATION` (-1) for a native method; 0 for a Java method
    /// with bytecode a reviewed `Intrinsic` shadows (interpreter round i6
    /// wave 120, lane L1, i40, [`shadow_named_by_invoke`]).
    entry_offset: u64,
    /// The exit's code index: -1 for a native method; the shadowed method's
    /// last return instruction's index, as HotSpot's interpreter reports the
    /// exit at the return that ran.
    exit_offset: u64,
}

#[cfg(feature = "experimental-debug")]
impl DebuggedNative {
    /// The listing's head while the thread is suspended at the event whose
    /// code index is `offset` (one of [`Self::entry_offset`] and
    /// [`Self::exit_offset`]): the native at -1, or (wave 120, i40) the
    /// shadowed Java method at `offset`.
    fn head(&self, offset: u64) -> super::NativeHead {
        if offset == crate::debug::NATIVE_FRAME_LOCATION {
            super::NativeHead::leaf(self.class_id, self.method_id)
        } else {
            super::NativeHead::located(
                self.class_id,
                self.method_id,
                i64::try_from(offset).unwrap_or(-1),
            )
        }
    }

    /// Is the native's return `out` reported by [`report_native_method_exit`]?
    /// A normal return always is (JDWP and JVMTI); an exceptional one only to
    /// a JVMTI agent, as `MethodExit` with `was_popped_by_exception` — HotSpot's
    /// JDWP back end reports no exit for a method that ended by an exception.
    pub(crate) fn reports_exit(&self, out: &crate::error::MethodCallResult) -> bool {
        match out {
            Ok(_) => true,
            Err(crate::error::MethodCallFailed::ExceptionThrown(_)) => {
                self.jvmti_method_id.is_some()
            }
            Err(_) => false,
        }
    }
}

/// Pin the object `out` carries — a returned reference or a thrown exception
/// — across a park or an agent callback on this thread (wave 27, lifted out of
/// [`report_native_method_exit`]'s wave-26 park): the pending-return slot that
/// also roots it is the next native call's, and a debugger invocation or a
/// JNI call from the callback makes native calls on this thread. Answers the
/// pin's index and whether the pending-return slot was set, for
/// [`unpin_native_result`].
#[cfg(feature = "experimental-debug")]
fn pin_native_result(
    thread: &mut JvmThread,
    out: &crate::error::MethodCallResult,
) -> Option<(usize, bool)> {
    let object = match out {
        Ok(Some(Value::Object(Some(o)))) => *o,
        Err(crate::error::MethodCallFailed::ExceptionThrown(e)) => *e,
        _ => return None,
    };
    let at = thread.native_pin_roots.len();
    thread.native_pin_roots.push(object);
    Some((at, thread.native_pending_return.is_some()))
}

/// Undo [`pin_native_result`]: write the pinned object back where a
/// collection moved it — into `out` and, when it was set, the pending-return
/// slot — and drop the pin.
#[cfg(feature = "experimental-debug")]
fn unpin_native_result(
    thread: &mut JvmThread,
    out: &mut crate::error::MethodCallResult,
    pin: Option<(usize, bool)>,
) {
    let Some((at, had_pending_return)) = pin else {
        return;
    };
    if let Some(moved) = thread.native_pin_roots.get(at).copied() {
        match out {
            Ok(Some(Value::Object(Some(o)))) => *o = moved,
            Err(crate::error::MethodCallFailed::ExceptionThrown(e)) => *e = moved,
            _ => {}
        }
        if had_pending_return {
            thread.native_pending_return = Some(moved);
        }
    }
    thread.native_pin_roots.truncate(at);
}

/// The native-call funnel is about to run `callback` (interpreter round i1
/// wave 26, lane L1): when it runs the `ACC_NATIVE` method the top
/// interpreter frame's invoke names ([`callback_runs_method`]: its registered
/// callback or, wave 27, its intrinsic trampoline), post that method's JVMTI
/// `MethodEntry` to the envs listening for it (wave 27), then report its JDWP
/// `MethodEntry` at location -1 to the requests it matches, applying the
/// set's suspend policy — a suspended thread parks here, its native listed
/// first above its caller ([`super::park_for_debugger_under`]) — and answer
/// the method for [`report_native_method_exit`]. `None` for anything else
/// (see the section note), for a native HotSpot posts no event for
/// ([`hotspot_posts_no_method_events`]), and while neither a method event
/// request is in force nor an env listens.
///
/// Called by `vm_exec::safe_native_call_impl` behind
/// `DebuggerGates::method_events_armed`, after it pinned the arguments and
/// before it enters the native thread state: the thread is still a mutator
/// here, a park polls safepoints, an agent's callback may collect, and the
/// funnel re-reads its arguments from their pins when this answers `parked`.
/// Takes the class-manager lock and the debug-state lock in turn, never
/// nested.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn report_native_method_entry(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: usize,
) -> Option<DebuggedNative> {
    let gates = &shared.debug.debugger_gates;
    // Work the debugger runs on a parked thread, or the server thread, posts
    // no JDWP event; a JVMTI agent (wave 27) sees it, as it sees the frames
    // that work pushes (`fire_method_entry_after_push`).
    let jdwp = gates.jdwp_method_events_armed() && !crate::debug::debugger_hooks_suppressed();
    let jvmti = gates.jvmti_method_events_armed();
    if !jdwp && !jvmti {
        return None;
    }
    // Wave 29: the method a reflective or method-handle door named for this
    // call, taken before anything else so that no nested call inherits it.
    let held = take_held_native(callback);
    let (caller_id, caller_pc) = {
        let top = thread.frames.last()?;
        (top.class_id, top.last_instr_pc)
    };
    // A compiled activation above the top interpreter frame made this call:
    // that frame's invoke is not the call in progress. Wave 28 (lane L1): the
    // native is named from the innermost compiled activation's invoke instead,
    // confirmed by the callback (`native_above_compiled_row`, which names a
    // blocked thread's native the same way); until wave 28 such a call
    // reported nothing. The caller stays listed at its own invoke, below the
    // compiled activations. Wave 29: when neither invoke names a method the
    // callback runs, the held method, if any.
    let compiled = super::capture_blocked_compiled_view(&thread.frames);
    let mut offsets = NATIVE_OFFSETS;
    let mut jvmti = jvmti;
    let (class_id, method_id) = match compiled.rows.last() {
        // Widening: a frame index
        Some(row) if row.below as usize >= thread.frames.len() => {
            super::native_above_compiled_row(shared, row, callback).or(held)?
        }
        _ => {
            let top = thread.frames.last()?;
            let named =
                super::native_named_by_invoke(shared, caller_id, &top.code, caller_pc, callback, true)
                    .or(held);
            match named {
                Some(named) => named,
                // Interpreter round i6 wave 120, lane L1 (i40,
                // `CRATONVM_JDWP_INTRINSIC_SHADOW_METHOD_EVENTS`, default
                // off): the invoke names a Java method with bytecode that a
                // reviewed `Intrinsic` answers without running it
                // (`stood_in_bytecode_may_run` refused it above). HotSpot's
                // interpreter runs that bytecode and reports its entry at
                // index 0 and its exit at the return that ran: report both
                // at those indexes, the method listed on top as an opaque
                // row. JDWP only (a C agent's table would list it at -1),
                // for an interpreted caller only (this arm).
                None if jdwp && gates.intrinsic_shadow_method_events() => {
                    let (class_id, method_id, exit) =
                        super::shadow_named_by_invoke(shared, caller_id, &top.code, caller_pc, callback)?;
                    offsets = (0, exit);
                    jvmti = false;
                    (class_id, method_id)
                }
                None => return None,
            }
        }
    };
    if crate::runtime::env_cache::frame_trace() && held == Some((class_id, method_id)) {
        eprintln!("[HELD_NATIVE] class={class_id} method_id={method_id:#x}");
    }
    report_named_native_method_entry(
        shared, thread, class_id, method_id, caller_pc, offsets, jdwp, jvmti,
    )
}

/// The entry and exit code indexes of a native method's events: -1 for both.
#[cfg(feature = "experimental-debug")]
const NATIVE_OFFSETS: (u64, u64) = (
    crate::debug::NATIVE_FRAME_LOCATION,
    crate::debug::NATIVE_FRAME_LOCATION,
);

/// The index of the last return instruction (`ireturn` .. `return`,
/// 0xac..=0xb1) of `code`, walked instruction by instruction: where the
/// frame-only report of a reviewed-`Intrinsic` shadow places its `MethodExit`
/// (interpreter round i6 wave 120, lane L1, i40). HotSpot reports the exit at
/// the return that ran; a method with one return (`Class.getName`, the
/// probe's) has no other. `None` for a body with no return.
#[cfg(feature = "experimental-debug")]
pub(super) fn last_return_index(code: &[u8]) -> Option<u64> {
    let mut pc = 0usize;
    let mut last = None;
    while pc < code.len() {
        if (0xac..=0xb1).contains(&code[pc]) {
            last = Some(pc);
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    last.and_then(|pc| u64::try_from(pc).ok())
}

/// A JNI-bound native — bound by `RegisterNatives` or resolved by the JNI
/// naming convention in a loaded library — is about to run
/// (interpreter round i1 wave 28, lane L1; stages 1-3 of
/// `docs/internal/fixed-bugs/interpreter-L1-proposal-method-events-of-jni-bound-natives-FIXED-20260929.md`):
/// report its `MethodEntry`, as [`report_native_method_entry`] reports a
/// registered native's. The two JNI arms of
/// `vm_exec::invoke_on_class_shared_inner` call `native::jni::dispatch_jni_native`
/// directly, never the native-call funnel, so until wave 28 a debugger or an
/// agent saw nothing for JNA's `Native.invoke*`, netty-tcnative's `SSL.*` or a
/// user's own `native` method. The arm holds the method it calls
/// (`declaring`, `name`, `descriptor`), so nothing is named from an invoke.
///
/// Called behind `DebuggerGates::method_events_armed`, before the arm installs
/// the JNI context and the implicit local frame, so a park (a suspending
/// request) or an agent's callback runs in the thread's ordinary state. The
/// object arguments are pinned across the report; when it parked or ran a
/// callback (either may collect) the second half of the answer is the
/// arguments re-read from those pins, which the arm passes to the native in
/// place of `args`. `None` when nothing listens, for the work the debugger
/// runs on a parked thread (JDWP only), and for a native HotSpot posts no
/// event for. The exit is [`report_native_method_exit`], when
/// [`DebuggedNative::reports_exit`] says so.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn report_jni_native_method_entry(
    shared: &SharedVm,
    thread: &mut JvmThread,
    declaring: ClassId,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<(DebuggedNative, Option<Vec<Value>>)> {
    let gates = &shared.debug.debugger_gates;
    let jdwp = gates.jdwp_method_events_armed() && !crate::debug::debugger_hooks_suppressed();
    let jvmti = gates.jvmti_method_events_armed();
    if !jdwp && !jvmti {
        return None;
    }
    // The caller is listed at its invoke while the thread is suspended here.
    let caller_pc = thread.frames.last().map_or(0, |top| top.last_instr_pc);
    let pin_base = thread.native_pin_roots.len();
    for value in args {
        if let Value::Object(Some(object)) = value {
            thread.native_pin_roots.push(*object);
        }
    }
    let native = report_named_native_method_entry(
        shared,
        thread,
        u64::from(declaring.as_u32()),
        crate::debug::jdwp_method_id(name, descriptor),
        caller_pc,
        NATIVE_OFFSETS,
        jdwp,
        jvmti,
    );
    let refreshed: Option<Vec<Value>> = match &native {
        Some(reported) if reported.parked => {
            let pins = &thread.native_pin_roots;
            let mut at = pin_base;
            Some(
                args.iter()
                    .map(|value| match value {
                        Value::Object(Some(object)) => {
                            let moved = pins.get(at).copied().unwrap_or(*object);
                            at += 1;
                            Value::Object(Some(moved))
                        }
                        other => *other,
                    })
                    .collect(),
            )
        }
        _ => None,
    };
    thread.native_pin_roots.truncate(pin_base);
    native.map(|reported| (reported, refreshed))
}

/// JDWP `MethodEntry` (`exit` `None`), or `MethodExit` /
/// `MethodExitWithReturnValue` (`exit` the return type's first byte and the
/// value), of method `index` of lambda proxy `proxy`, which a call recorded
/// with its frame runs (`interpreter::lambda`'s proxy dispatch, the
/// constructor at the instance's creation; the i54-L1 retirement): HotSpot's
/// back end reports the spun class's methods (MEASURED,
/// `tools/probes/interp/L1/L1K54JdiLambdaProxyEvents.java`), the entry at
/// code index 0 and the exit at the method's `return`. The proxy method's row
/// is pushed, so a suspending request parks the thread with the proxy's frame
/// on top of its caller. Answers whether the thread parked (a collection may
/// have moved what it holds). Behind the method-event gate.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn report_lambda_proxy_method_event(
    shared: &SharedVm,
    thread: &mut JvmThread,
    proxy: ClassId,
    index: u16,
    exit: Option<(u8, Option<Value>)>,
) -> bool {
    use crate::debug::events::{EventKind, EventLocation};
    if !shared.debug.debugger_gates.jdwp_method_events_armed()
        || crate::debug::debugger_hooks_suppressed()
    {
        return false;
    }
    let (entries, exits) = shared
        .debug
        .debug_state
        .lock()
        .events
        .method_events_requested();
    if (exit.is_none() && !entries) || (exit.is_some() && !exits) {
        return false;
    }
    let Some(row) = shared
        .debug
        .lambda_proxy_rows
        .lock()
        .get(&(proxy, index))
        .cloned()
    else {
        return false;
    };
    let Some(class_name) = crate::debug::lambda_proxy_internal_name(shared, proxy) else {
        return false;
    };
    let supertypes = crate::debug::lambda_proxy_supertypes(shared, proxy);
    let method_id = crate::debug::jdwp_method_id(&row.name, &row.descriptor);
    let offset = match exit {
        None => 0,
        // The spun code ends with its one `return`.
        Some(_) => {
            crate::runtime::lambda_proxy::lambda_proxy_code(shared, proxy, usize::from(index))
                .and_then(|code| u64::try_from(code.len().saturating_sub(1)).ok())
                .unwrap_or(0)
        }
    };
    let object_tag = match exit {
        Some((_, Some(Value::Object(Some(o))))) => {
            let cm = shared.classes.class_manager.read();
            Some(crate::debug::inspect::object_tag(shared, &cm, o))
        }
        _ => None,
    };
    let tid = thread.thread_id.0;
    let class_id = u64::from(proxy.as_u32());
    let location = EventLocation {
        class_id,
        class_name: &class_name,
        method_id,
        offset,
        thread_id: tid,
        frame_depth: thread.frames.len() + 1,
        line_start: None,
        line: None,
    };
    let caller_pc = thread.frames.last().map_or(0, |top| top.last_instr_pc);
    let kinds: &[EventKind] = if exit.is_some() {
        &[EventKind::MethodExit, EventKind::MethodExitWithReturnValue]
    } else {
        &[EventKind::MethodEntry]
    };
    let (events, park) = {
        let mut ds = shared.debug.debug_state.lock();
        let class_is = |id: u64| supertypes.contains(&id);
        let mut events = Vec::new();
        for &kind in kinds {
            for (request_id, suspend_policy) in
                ds.events.match_location_events(kind, &location, &class_is)
            {
                let mut extra = Vec::new();
                if let (EventKind::MethodExitWithReturnValue, Some((return_type, value))) =
                    (kind, exit)
                {
                    let mut pw = crate::debug::protocol::PayloadWriter::new();
                    match value {
                        Some(v) => crate::debug::inspect::put_tagged_with_tag(
                            shared,
                            &mut ds,
                            &mut pw,
                            return_type,
                            v,
                            object_tag,
                        ),
                        None => pw.put_u8(b'V'),
                    }
                    extra = pw.into_bytes();
                }
                events.push(crate::debug::DebugEvent {
                    kind,
                    request_id,
                    suspend_policy,
                    thread_id: tid,
                    class_id,
                    method_id,
                    offset,
                    extra,
                    set_follows: false,
                });
            }
        }
        crate::debug::republish_gates_if_spent(shared, &mut ds);
        let park = !events.is_empty() && {
            let policy =
                crate::debug::strongest_suspend_policy(events.iter().map(|e| e.suspend_policy));
            if ds.apply_event_set_policy(policy, tid) {
                crate::debug::publish_debugger_gates(shared, &ds);
            }
            let suspended = ds.is_thread_suspended(tid);
            if suspended {
                super::publish_debugger_frames_under(shared, thread, &mut ds, tid, caller_pc, None);
            }
            suspended
        };
        (events, park)
    };
    super::send_debugger_events(shared, events);
    if park {
        super::park_for_debugger_under(shared, thread, tid, caller_pc, None);
    }
    park
}

/// [`report_native_method_entry`] once the native method is named: `(class_id,
/// method_id)` — wire class id and JDWP method id — of the method, and
/// `caller_pc`, the invoke its caller is listed at while the thread is
/// suspended here; `jdwp` / `jvmti` are the gate's halves as the caller read
/// them (interpreter round i1 wave 28, lane L1: split out so the JNI arms of
/// `vm_exec::invoke_on_class_shared_inner`, which hold the method they call,
/// report through it — [`report_jni_native_method_entry`]).
#[cfg(feature = "experimental-debug")]
fn report_named_native_method_entry(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: u64,
    method_id: u64,
    caller_pc: usize,
    (entry_offset, exit_offset): (u64, u64),
    jdwp: bool,
    jvmti: bool,
) -> Option<DebuggedNative> {
    use crate::debug::events::EventKind;
    let ((entries, _), filter_classes) = if jdwp {
        let ds = shared.debug.debug_state.lock();
        (
            ds.events.method_events_requested(),
            ds.events.location_filter_class_ids(),
        )
    } else {
        ((false, false), Vec::new())
    };
    let declaring = ClassId::new(u32::try_from(class_id).ok()?);
    let (class_name, return_type, location_supers, name, descriptor) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(declaring)?;
        let method = class
            .methods
            .iter()
            .find(|m| crate::debug::jdwp_method_id(&m.name, &m.descriptor) == method_id)?;
        if hotspot_posts_no_method_events(&class.name, &method.name, &method.descriptor) {
            return None;
        }
        let descriptor: &str = &method.descriptor;
        let return_type = descriptor
            .rsplit(')')
            .next()
            .and_then(|ret| ret.bytes().next())
            .unwrap_or(b'V');
        let supers = if filter_classes.is_empty() {
            Vec::new()
        } else {
            super::debugger_supertypes_among(&cm, declaring, &filter_classes)
        };
        (
            std::sync::Arc::clone(&class.name),
            return_type,
            supers,
            std::sync::Arc::clone(&method.name),
            std::sync::Arc::clone(&method.descriptor),
        )
    };
    // The JVMTI id after the class-manager lock is released: the resolution
    // takes it again.
    let vm = shared.vm_identity;
    let jvmti_method_id = if jvmti {
        crate::runtime::jvmti::resolve_method_id_for_vm(vm, declaring.as_u32(), &name, &descriptor)
    } else {
        None
    };
    let mut native = DebuggedNative {
        class_id,
        method_id,
        class_name,
        return_type,
        caller_pc,
        location_supers,
        jdwp,
        jvmti_method_id,
        parked: false,
        entry_offset,
        exit_offset,
    };
    let tid = thread.thread_id.0;
    // JVMTI `MethodEntry` (wave 27), to the envs that enabled it for this
    // thread (`fire_method_entry_for_vm` re-checks). The agent's callback may
    // call into Java and collect, so the funnel re-reads its arguments.
    if let Some(jvmti_id) = native.jvmti_method_id {
        if crate::runtime::jvmti::any_method_entry_listener_active_for_vm(vm) {
            // Wave 42: the native is at depth 0 of the agent's stack functions
            // during its own `MethodEntry`, as on HotSpot (a JNI arm recorded
            // it already; `enter_hashed` then adds nothing). The JDWP method
            // id is the row's key (`native_row_method_hash`).
            let own = thread as *mut JvmThread;
            // SAFETY: this thread's own `JvmThread`, which outlives the row.
            let _row = unsafe {
                crate::jvmti::native_env::NativeFrameRow::enter_hashed(shared, own, declaring, method_id)
            };
            // gci i11/ja: the callback reborrows the row's pointer (one
            // provenance, `thread` unused until both end).
            // SAFETY: `own` is derived from this frame's live `&mut`.
            unsafe {
                post_on_own_thread_ptr(own, || {
                    crate::runtime::jvmti::fire_method_entry_for_vm(vm, tid, jvmti_id)
                })
            };
            native.parked = true;
        }
    }
    if !entries {
        return Some(native);
    }
    let location = native_location(&native, native.entry_offset, tid, thread.frames.len());
    let (events, park) = {
        let mut ds = shared.debug.debug_state.lock();
        let class_is = |id: u64| native.location_supers.contains(&id);
        let events: Vec<crate::debug::DebugEvent> = ds
            .events
            .match_location_events(EventKind::MethodEntry, &location, &class_is)
            .into_iter()
            .map(|(request_id, suspend_policy)| {
                native_event(
                    &native,
                    EventKind::MethodEntry,
                    request_id,
                    suspend_policy,
                    tid,
                )
            })
            .collect();
        // Wave 46 (lane L1): a `Count` this match spent releases its gates.
        crate::debug::republish_gates_if_spent(shared, &mut ds);
        let park = !events.is_empty()
            && apply_native_event_policies(
                shared,
                thread,
                &mut ds,
                tid,
                &native,
                native.entry_offset,
                &events,
            );
        (events, park)
    };
    super::send_debugger_events(shared, events);
    if park {
        super::park_for_debugger_under(
            shared,
            thread,
            tid,
            native.caller_pc,
            Some(native.head(native.entry_offset)),
        );
        native.parked = true;
    }
    Some(native)
}

/// The native `native` returned with `out` (interpreter round i1 wave 26,
/// lane L1): report its JDWP `MethodExit` / `MethodExitWithReturnValue` at
/// location -1 — the value tagged by the method's return type — applying the
/// set's suspend policy, and (wave 27) post its JVMTI `MethodExit` to the envs
/// that enabled it for this thread. HotSpot's JDWP back end reports no exit
/// for a method that ended by an exception, and neither does this; JVMTI
/// does, with `was_popped_by_exception` set. The funnel calls it when
/// [`DebuggedNative::reports_exit`] says so.
///
/// Called by `vm_exec::safe_native_call_impl` after the callback, with the
/// native thread state already restored, the arguments still pinned and an
/// object result or a thrown exception in `JvmThread::native_pending_return`.
/// A park or an agent's callback pins that object (a debugger invocation or a
/// JNI call on this thread reuses the pending-return slot) and writes it back
/// where a collection moved it, into `out` and the slot.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn report_native_method_exit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    native: &DebuggedNative,
    out: &mut crate::error::MethodCallResult,
) {
    use crate::debug::events::{EventKind, EventLocation};
    let tid = thread.thread_id.0;
    if let Some(jvmti_id) = native.jvmti_method_id {
        let vm = shared.vm_identity;
        if crate::runtime::jvmti::any_method_exit_listener_active_for_vm(vm) {
            let (popped, value) = match &*out {
                Ok(returned) => (
                    false,
                    to_local_value(native_return_value(native.return_type, *returned).as_ref()),
                ),
                Err(_) => (true, crate::runtime::jvmti::LocalValue::Object(None)),
            };
            let pin = pin_native_result(thread, out);
            // Wave 42: the native is at depth 0 during its own `MethodExit`,
            // as in its entry's post (`report_named_native_method_entry`).
            if let Ok(class_id) = u32::try_from(native.class_id) {
                let own = thread as *mut JvmThread;
                // SAFETY: this thread's own `JvmThread`, which outlives the row.
                let _row = unsafe {
                    crate::jvmti::native_env::NativeFrameRow::enter_hashed(
                        shared,
                        own,
                        ClassId::new(class_id),
                        native.method_id,
                    )
                };
                // gci i11/ja: as in the entry's post.
                // SAFETY: `own` is derived from this frame's live `&mut`.
                unsafe {
                    post_on_own_thread_ptr(own, || {
                        crate::runtime::jvmti::fire_method_exit_for_vm(
                            vm, tid, jvmti_id, popped, value,
                        )
                    })
                };
            } else {
                // gci i11/ja: the callback reborrows this frame's `&mut`.
                post_on_own_thread(thread, || {
                    crate::runtime::jvmti::fire_method_exit_for_vm(vm, tid, jvmti_id, popped, value)
                });
            }
            unpin_native_result(thread, out, pin);
        }
    }
    if !native.jdwp || crate::debug::debugger_hooks_suppressed() {
        return;
    }
    let returned = match &*out {
        Ok(v) => *v,
        Err(_) => return,
    };
    let (_, exits) = shared
        .debug
        .debug_state
        .lock()
        .events
        .method_events_requested();
    if !exits {
        return;
    }
    let value = native_return_value(native.return_type, returned);
    let object_tag = match value {
        Some(Value::Object(Some(o))) => {
            let cm = shared.classes.class_manager.read();
            Some(crate::debug::inspect::object_tag(shared, &cm, o))
        }
        _ => None,
    };
    let location: EventLocation<'_> =
        native_location(native, native.exit_offset, tid, thread.frames.len());
    let (events, park) = {
        let mut ds = shared.debug.debug_state.lock();
        let class_is = |id: u64| native.location_supers.contains(&id);
        let mut events = Vec::new();
        for kind in [EventKind::MethodExit, EventKind::MethodExitWithReturnValue] {
            let matched = ds.events.match_location_events(kind, &location, &class_is);
            for (request_id, suspend_policy) in matched {
                let mut event = native_event(native, kind, request_id, suspend_policy, tid);
                if kind == EventKind::MethodExitWithReturnValue {
                    let mut pw = crate::debug::protocol::PayloadWriter::new();
                    match value {
                        Some(v) => crate::debug::inspect::put_tagged_with_tag(
                            shared,
                            &mut ds,
                            &mut pw,
                            native.return_type,
                            v,
                            object_tag,
                        ),
                        None => pw.put_u8(b'V'),
                    }
                    event.extra = pw.into_bytes();
                }
                events.push(event);
            }
        }
        // Wave 46 (lane L1): a `Count` this match spent releases its gates.
        crate::debug::republish_gates_if_spent(shared, &mut ds);
        let park = !events.is_empty()
            && apply_native_event_policies(
                shared,
                thread,
                &mut ds,
                tid,
                native,
                native.exit_offset,
                &events,
            );
        (events, park)
    };
    super::send_debugger_events(shared, events);
    if !park {
        return;
    }
    // Rooted across the park by a pin of its own: the pending-return slot is
    // the next native call's, and a debugger invocation run from the park
    // makes native calls on this thread.
    let pin = pin_native_result(thread, out);
    super::park_for_debugger_under(
        shared,
        thread,
        tid,
        native.caller_pc,
        Some(native.head(native.exit_offset)),
    );
    unpin_native_result(thread, out, pin);
}

/// Park a thread the debugger suspended while it ran a native method at that
/// native's return, with the native listed on top (interpreter round i1 wave
/// 45, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-a-thread-suspended-inside-a-native-method-parks-past-it-FIXED-20261009.md`).
/// `false`: wave 44's behaviour (the thread runs on out of the native and
/// parks at its caller's next interpreter suspend point, the caller on top).
#[cfg(feature = "experimental-debug")]
pub(crate) const NATIVE_EXIT_SUSPENSION_PARK_ENABLED: bool = true;

/// Which native method a thread returns from, for
/// [`park_if_suspended_at_native_exit`].
#[cfg(feature = "experimental-debug")]
#[derive(Clone, Copy)]
pub(crate) enum ReturningNative {
    /// A registered native or intrinsic trampoline the native-call funnel
    /// ran: named from its caller's invoke, confirmed by this callback
    /// address, as `report_native_method_entry` names it.
    Callback(usize),
    /// A JNI-bound native the JNI arms of `vm_exec` ran: `(class id, JDWP
    /// method id)`, which those arms hold.
    Named(u64, u64),
}

/// A native method returned with `out` on a thread the debugger suspended
/// while the native ran (interpreter round i1 wave 45, lane L1): park the
/// thread here, before it returns to its caller, with the native listed on
/// top at location -1 above its caller at the invoke, as HotSpot holds a
/// thread suspended in native code at its transition back to Java.
///
/// Until wave 45 the thread ran on into its caller and parked at the next
/// interpreter suspend point, so its listing lost the native, and
/// `ForceEarlyReturn` and `PopFrames` were served on the caller where
/// HotSpot answers `OPAQUE_FRAME` (`L1W43RawJdwpObjectErrorAnswers`'
/// `forceEarlyReturn(running other)`, which answered 34 for an `int` value
/// on a `void` caller). This park is not a suspend point before a bytecode
/// (`debug::early_return::enter_point` is not called), so those two commands
/// answer `OPAQUE_FRAME` here, and the local commands answer it for the
/// native's row (`commands::is_native_frame`).
///
/// Called by the native-call funnel (`vm_exec::safe_native_call_impl`) and
/// the JNI arms after the native returned, behind
/// `DebuggerGates::suspension_in_force` (one load, in a build with a JDWP
/// server only), with the thread's native state closed, the arguments still
/// pinned and an object result or thrown exception rooted in the pending-return
/// slot; the result is pinned across the park as
/// [`report_native_method_exit`] pins it. Nothing for a thread that is not
/// suspended, for the work the debugger runs on a parked thread, for an
/// outcome that is neither a return nor an exception (a virtual thread's
/// yield), and for a native that cannot be named: the thread then parks at
/// its next interpreter suspend point, as before.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn park_if_suspended_at_native_exit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    native: ReturningNative,
    out: &mut crate::error::MethodCallResult,
) {
    if !NATIVE_EXIT_SUSPENSION_PARK_ENABLED || crate::debug::debugger_hooks_suppressed() {
        return;
    }
    if !matches!(
        out,
        Ok(_) | Err(crate::error::MethodCallFailed::ExceptionThrown(_))
    ) {
        return;
    }
    let tid = thread.thread_id.0;
    if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return;
    }
    let Some(caller_pc) = thread.frames.last().map(|top| top.last_instr_pc) else {
        return;
    };
    // Named before the debug-state lock, which is taken after the class
    // manager's; the compiled activations are walked here, on the thread.
    let compiled = super::capture_blocked_compiled_view(&thread.frames);
    // Named as `report_native_method_entry` names it: from the innermost
    // compiled activation's invoke, or the top interpreter frame's, confirmed
    // by the callback even for a fixed target, so that a native another
    // native's callback runs from Rust (the top frame's invoke names the
    // outer one) is not taken for the one that invoke names.
    let named = match native {
        ReturningNative::Callback(callback) => match compiled.rows.last() {
            // Widening: a frame index
            Some(row) if row.below as usize >= thread.frames.len() => {
                super::native_above_compiled_row(shared, row, callback)
            }
            _ => thread.frames.last().and_then(|top| {
                super::native_named_by_invoke(
                    shared,
                    top.class_id,
                    &top.code,
                    top.last_instr_pc,
                    callback,
                    true,
                )
            }),
        },
        ReturningNative::Named(class_id, method_id) => Some((class_id, method_id)),
    };
    // Wave 46 (lane L1): a blocking stand-in (`Thread.sleep`, `Object.wait`)
    // parks with its native leaf on top, as HotSpot holds it; since wave 47
    // with the JDK Java levels between the leaf and the caller, and the park
    // family (`LockSupport.park*`) too ([`blocking_standin_head`]).
    let head = named
        .map(|(class_id, method_id)| super::NativeHead::leaf(class_id, method_id))
        .or_else(|| match native {
            ReturningNative::Callback(callback) => match compiled.rows.last() {
                // Wave 47: a compiled caller (a polling loop the JIT
                // compiled) names the stand-in from its innermost
                // activation's invoke.
                // Widening: a frame index
                Some(row) if row.below as usize >= thread.frames.len() => {
                    super::standin_head_above_compiled_row(shared, row, callback)
                }
                _ => thread.frames.last().and_then(|top| {
                    blocking_standin_head(
                        shared,
                        top.class_id,
                        &top.code,
                        top.last_instr_pc,
                        Some(callback),
                    )
                }),
            },
            ReturningNative::Named(..) => None,
        });
    let Some(head) = head else {
        // Interpreter round i2 wave 53, lane L6: a thread that acknowledged
        // the suspension in this native (`CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`)
        // was counted stopped by the suspension's reply, so it must not run
        // its caller's next bytecodes: it parks here, its caller on top at
        // the invoke. Not for a native another native's Rust runs (the outer
        // one is still recorded, `JvmThread::running_native`): that one may
        // hold a VM lock, and the outer native's return parks.
        if shared.debug.debugger_gates.suspend_native_ack() && thread.running_native == 0 {
            park_unnamed_native_exit(shared, thread, tid, caller_pc, compiled, out);
            return;
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[NATIVE_EXIT_PARK] unnamed tid={tid}: parks at the next suspend point");
        }
        return;
    };
    let native_top = head.native();
    let status = native_exit_status(shared, native_top);
    // Wave 47: the JNI natives and reflective calls below, named before the
    // debug-state lock.
    let splices = super::debugger_listing_splices(shared, thread, Some(&head), true);
    {
        let mut ds = shared.debug.debug_state.lock();
        if !ds.is_thread_suspended(tid) {
            return;
        }
        if let Some(status) = status {
            ds.native_exit_statuses.insert(tid, status);
        }
        super::publish_frame_snapshot(
            shared,
            &thread.frames,
            &mut ds,
            tid,
            caller_pc,
            Some(head),
            Some(&compiled),
            &splices,
            |_, _, v| v,
        );
    }
    drop(compiled);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[NATIVE_EXIT_PARK] tid={tid} class={} method_id={:#x} depth={}",
            native_top.0,
            native_top.1,
            thread.frames.len()
        );
    }
    let pin = pin_native_result(thread, out);
    // The native is also at depth 0 of the C JVMTI table's stack functions
    // while the thread is parked here (wave 45: `NotifyFramePop` and the local
    // functions run on the parked thread and count its frames that way). A
    // JNI arm's own row already names it (`enter_hashed` then adds nothing).
    let row = match (native, u32::try_from(native_top.0).ok().map(ClassId::new)) {
        (ReturningNative::Callback(_), Some(declaring)) => Some(
            // SAFETY: this thread's own `JvmThread`, which outlives the row.
            unsafe {
                crate::jvmti::native_env::NativeFrameRow::enter_hashed(
                    shared,
                    thread as *mut JvmThread,
                    declaring,
                    native_top.1,
                )
            },
        ),
        _ => None,
    };
    super::park_for_debugger_under(shared, thread, tid, caller_pc, Some(head));
    drop(row);
    unpin_native_result(thread, out, pin);
    if status.is_some() {
        shared
            .debug
            .debug_state
            .lock()
            .native_exit_statuses
            .remove(&tid);
    }
}

/// [`park_if_suspended_at_native_exit`] for a native it cannot name, under
/// `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK` (interpreter round i2 wave 53, lane L6):
/// the listing has no native row, its top is the caller at the invoke
/// (`caller_pc`), with the compiled activations it stands under; the result
/// is pinned across the park as for a named native. Before wave 53 such a
/// thread ran on into its caller and parked at the next interpreter suspend
/// point, which a suspension that counted it stopped in the native must not
/// allow.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
fn park_unnamed_native_exit(
    shared: &SharedVm,
    thread: &mut JvmThread,
    tid: u64,
    caller_pc: usize,
    compiled: crate::threading::jvm_thread::BlockedCompiledView,
    out: &mut crate::error::MethodCallResult,
) {
    let splices = super::debugger_listing_splices(shared, thread, None, true);
    {
        let mut ds = shared.debug.debug_state.lock();
        if !ds.is_thread_suspended(tid) {
            return;
        }
        super::publish_frame_snapshot(
            shared,
            &thread.frames,
            &mut ds,
            tid,
            caller_pc,
            None,
            Some(&compiled),
            &splices,
            |_, _, v| v,
        );
    }
    drop(compiled);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[NATIVE_EXIT_PARK] unnamed tid={tid} depth={}: parks at the native's return",
            thread.frames.len()
        );
    }
    let pin = pin_native_result(thread, out);
    super::park_for_debugger_under(shared, thread, tid, caller_pc, None);
    unpin_native_result(thread, out, pin);
}

/// The JDWP `ThreadStatus` HotSpot reports for a thread held at the return
/// of native `(class id, JDWP method id)`, when it is not `RUNNING` (wave 45):
/// `WAIT` for `Object.wait0` (and `Object.wait`, where a registered native
/// stands in for it), measured on HotSpot 25.0.3
/// (`L1W45JdiSuspendedInNative`). `Thread.sleepNanos0` and `Unsafe.park`
/// read `RUNNING` there, as the registry reads this thread. Takes the
/// class-manager lock: call it before the debug-state lock.
#[cfg(feature = "experimental-debug")]
fn native_exit_status(shared: &SharedVm, (class_id, method_id): (u64, u64)) -> Option<u32> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(ClassId::new(u32::try_from(class_id).ok()?))?;
    if &*class.name != "java/lang/Object" {
        return None;
    }
    class
        .methods
        .iter()
        .any(|m| {
            (&*m.name == "wait0" || &*m.name == "wait")
                && crate::debug::jdwp_method_id(&m.name, &m.descriptor) == method_id
        })
        .then_some(crate::debug::commands::THREAD_STATUS_WAIT)
}

/// The value a native returned as its method's return type reads it: `void`
/// carries none (a void native may return a value for convenience), and a
/// native that returned nothing for a non-void method reads as `null` or 0.
#[cfg(feature = "experimental-debug")]
fn native_return_value(return_type: u8, returned: Option<Value>) -> Option<Value> {
    match (return_type, returned) {
        (b'V', _) => None,
        (_, Some(v)) => Some(v),
        (b'L' | b'[', None) => Some(Value::Object(None)),
        (_, None) => Some(Value::Int(0)),
    }
}

/// Where the events of `native` are matched: its method at `offset` (-1 for
/// a native method; wave 120, i40: a shadowed Java method's entry or exit
/// index), one frame above the thread's `interpreter_frames`.
#[cfg(feature = "experimental-debug")]
fn native_location(
    native: &DebuggedNative,
    offset: u64,
    tid: u64,
    interpreter_frames: usize,
) -> crate::debug::events::EventLocation<'_> {
    crate::debug::events::EventLocation {
        class_id: native.class_id,
        class_name: &*native.class_name,
        method_id: native.method_id,
        offset,
        thread_id: tid,
        frame_depth: interpreter_frames + 1,
        line_start: None,
        line: None,
    }
}

/// One event of `native`'s entry or exit set, for request `request_id`.
#[cfg(feature = "experimental-debug")]
fn native_event(
    native: &DebuggedNative,
    kind: crate::debug::events::EventKind,
    request_id: u32,
    suspend_policy: crate::debug::events::SuspendPolicy,
    tid: u64,
) -> crate::debug::DebugEvent {
    crate::debug::DebugEvent {
        kind,
        request_id,
        suspend_policy,
        thread_id: tid,
        class_id: native.class_id,
        method_id: native.method_id,
        // Wave 120 (i40): the entry's index for `MethodEntry`, the exit's
        // for both exit kinds (-1 for both on a native method).
        offset: if kind == crate::debug::events::EventKind::MethodEntry {
            native.entry_offset
        } else {
            native.exit_offset
        },
        extra: Vec::new(),
        set_follows: false,
    }
}

/// `apply_debugger_event_policies` for a native method's event set: the
/// set's strongest policy, once; then, if the thread is suspended, its frames
/// published with the native first, at location -1 (wave 120, i40: a
/// shadowed Java method at `offset`), above the caller at its invoke. Answers
/// whether the thread must park.
#[cfg(feature = "experimental-debug")]
fn apply_native_event_policies(
    shared: &SharedVm,
    thread: &JvmThread,
    ds: &mut crate::debug::DebugState,
    tid: u64,
    native: &DebuggedNative,
    offset: u64,
    events: &[crate::debug::DebugEvent],
) -> bool {
    let policy = crate::debug::strongest_suspend_policy(events.iter().map(|e| e.suspend_policy));
    if ds.apply_event_set_policy(policy, tid) {
        crate::debug::publish_debugger_gates(shared, ds);
    }
    let park = ds.is_thread_suspended(tid);
    if park {
        super::publish_debugger_frames_under(
            shared,
            thread,
            ds,
            tid,
            native.caller_pc,
            Some(native.head(offset)),
        );
    }
    park
}

// ---------------------------------------------------------------------------
// JDWP `ThreadReference.ForceEarlyReturn` (interpreter round i1 wave 43,
// lane L1)
// ---------------------------------------------------------------------------

/// What the debugger's suspend point (`deliver_breakpoint_if_set`) asks the
/// dispatch loop to do instead of running the bytecode at `pc`.
#[cfg_attr(not(feature = "experimental-debug"), allow(dead_code))]
pub(super) enum DebuggerStop {
    /// Throw this exception at `pc`: a `ThreadReference.Stop`, or one made
    /// while the thread was suspended at a method exit.
    Throw(ObjectRef),
    /// Return from the frame with the value on top of its operand stack
    /// (nothing for a `void` method), which [`force_return_here`] left
    /// there: a JDWP `ForceEarlyReturn` (wave 43). The loop finishes it with
    /// [`finish_forced_return`].
    ForcedReturn,
    /// Frames were popped while the thread was parked (a JDWP `PopFrames`,
    /// interpreter round i1 wave 44, lane L1; [`pop_frames_for_debugger`]):
    /// the loop runs on in the new top frame, at its invoke.
    FramesPopped,
}

/// What the dispatch loop does after [`finish_forced_return`].
pub(super) enum ForcedReturnStep {
    /// The frame was popped and its value pushed onto its caller's operand
    /// stack; the caller (`frame_idx - 1`) runs on.
    Popped,
    /// The frame is the loop's entry frame: the loop returns this value, as
    /// the return arms do.
    Returned(Option<Value>),
}

/// The value a JDWP `ForceEarlyReturn` recorded for frame `frame_idx`,
/// taken as the thread leaves its park at `pc` (`forced`: `None` for a
/// `void` method): pushed onto the frame's emptied operand stack, where it is
/// a root across anything that follows, and the frame's `MethodExit` events
/// reported with it, as HotSpot posts them for a forced return (JDWP
/// `MethodExit` / `MethodExitWithReturnValue`, a set of their own, which may
/// suspend the thread again). The dispatch loop then returns it
/// ([`DebuggerStop::ForcedReturn`], [`finish_forced_return`]).
///
/// The rest of the operand stack is discarded (the frame is leaving). A
/// value of a kind the return type does not take (`debug::early_return`
/// refuses one when it is recorded) leaves the frame untouched, to run on at
/// `pc`. A `ThreadReference.Stop` made while the thread was suspended at the
/// exit is thrown instead.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(super) fn force_return_here(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pc: usize,
    tid: u64,
    forced: Option<Value>,
    location_supers: &[u64],
) -> Option<DebuggerStop> {
    let frame = thread.frames.get_mut(frame_idx)?;
    let ret = frame.return_tag();
    // `debug::early_return` checked the kind against the return type; a
    // value of another kind leaves the frame untouched, to run on at `pc`.
    let fits = matches!(
        (ret, forced),
        (b'V', None)
            | (b'J', Some(Value::Long(_)))
            | (b'F', Some(Value::Float(_)))
            | (b'D', Some(Value::Double(_)))
            | (b'L' | b'[', Some(Value::Object(_)))
            | (b'Z' | b'B' | b'C' | b'S' | b'I', Some(Value::Int(_)))
    );
    if !fits {
        return None;
    }
    frame.stack.clear();
    // Cannot fail for verified code (a non-`void` method has a `max_stack`
    // of at least 1); if it did, `finish_forced_return` fails loudly on the
    // empty stack rather than running the frame on without its operands.
    let _ = match forced {
        Some(Value::Long(v)) => frame.stack.push_long(v),
        Some(Value::Float(v)) => frame.stack.push_float(v),
        Some(Value::Double(v)) => frame.stack.push_double(v),
        Some(Value::Int(v)) => frame.stack.push_int(v),
        Some(v) => frame.stack.push(v),
        None => Ok(()),
    };
    // The return bytecode a normal return of this type would run, for the
    // exit events' value (`debugger_return_value`).
    let return_opcode = match ret {
        b'V' => 0xb1,
        b'J' => 0xad,
        b'F' => 0xae,
        b'D' => 0xaf,
        b'L' | b'[' => 0xb0,
        _ => 0xac,
    };
    let (_, exits) = shared
        .debug
        .debug_state
        .lock()
        .events
        .method_events_requested();
    if exits {
        let stop = super::deliver_method_exit_events(
            shared,
            thread,
            frame_idx,
            pc,
            tid,
            Some(return_opcode),
            location_supers,
        );
        if let Some(exc) = stop {
            return Some(DebuggerStop::Throw(exc));
        }
    }
    Some(DebuggerStop::ForcedReturn)
}

/// Interpreter round i2 wave 62, lane L1 (stage 1 of
/// `docs/internal/proposals/interpreter/i61-L1-proposal-frame-control-from-an-event-callback-20261025.md`):
/// the thread is about to run a C agent's `Breakpoint` or `SingleStep`
/// callback for its top frame, which the dispatch loop whose entry frame is
/// `loop_base` runs. With `DebuggerGates::c_callback_frame_control` on, a
/// callback point is entered (`debug::early_return::enter_point`), at which
/// the callback's `PopFrame` / `ForceEarlyReturn*` of the calling thread is
/// recorded (`jvmti::native_env`'s `FrameTarget::Current`); answers whether
/// one was, so the caller leaves it ([`leave_callback_point`]). Off, two
/// loads and `false`.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(super) fn enter_callback_point(
    shared: &SharedVm,
    thread: &JvmThread,
    loop_base: usize,
) -> bool {
    if !shared.debug.debugger_gates.c_callback_frame_control() {
        return false;
    }
    crate::debug::early_return::enter_point(
        shared,
        thread.thread_id.0,
        thread.frames.len(),
        loop_base,
    );
    true
}

/// [`enter_callback_point`] in a build without the debugger surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(super) fn enter_callback_point(
    _shared: &SharedVm,
    _thread: &JvmThread,
    _loop_base: usize,
) -> bool {
    false
}

/// The callback [`enter_callback_point`] entered a point for returned:
/// leave the point; answers whether it recorded a `PopFrame` or a forced
/// return of the top frame, which [`take_callback_frame_control`] takes
/// before the bytecode runs (wave 62).
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(super) fn leave_callback_point(shared: &SharedVm, thread: &JvmThread) -> bool {
    let requested = crate::debug::early_return::leave_callback_point(
        shared,
        thread.thread_id.0,
        thread.frames.len(),
    );
    if requested && crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[CALLBACK_FRAME_CONTROL] requested tid={} depth={}",
            thread.thread_id.0,
            thread.frames.len()
        );
    }
    requested
}

/// [`leave_callback_point`] in a build without the debugger surface.
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(super) fn leave_callback_point(_shared: &SharedVm, _thread: &JvmThread) -> bool {
    false
}

/// Frames were just popped (a `PopFrame` taken at the suspend point): is a
/// C agent's forced return recorded for the frame now on top (a
/// `ForceEarlyReturn*` made after the `PopFrame` in the same stop, wave 62)?
/// Then the loop takes it at the caller's invoke, before the invoke runs
/// again. One lock of the debug state per pop.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(super) fn forced_return_follows_pop(shared: &SharedVm, thread: &JvmThread) -> bool {
    crate::debug::early_return::jvmti_pending_at(shared, thread.thread_id.0, thread.frames.len())
}

/// [`forced_return_follows_pop`] in a build without the debugger surface
/// (no frame is ever popped there).
#[cfg(not(feature = "experimental-debug"))]
#[inline(always)]
pub(super) fn forced_return_follows_pop(_shared: &SharedVm, _thread: &JvmThread) -> bool {
    false
}

/// Take what a callback recorded at its point (wave 62): a `PopFrame` pops
/// frame `frame_idx` and its caller re-runs the invoke
/// ([`DebuggerStop::FramesPopped`], `debug::pop_frames::take_jvmti_pop`); a
/// forced return leaves the value for the loop to return
/// ([`force_return_here`], [`DebuggerStop::ForcedReturn`]). `None` when
/// nothing was recorded for this frame (or a value of another kind, which
/// `debug::early_return::record_jvmti` refuses first): the bytecode at `pc`
/// runs.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(super) fn take_callback_frame_control(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    pc: usize,
) -> Option<DebuggerStop> {
    let tid = thread.thread_id.0;
    let depth = thread.frames.len();
    if crate::debug::pop_frames::take_jvmti_pop(shared, thread, tid, depth) {
        return Some(DebuggerStop::FramesPopped);
    }
    let forced = crate::debug::early_return::take(shared, tid, depth)?;
    // The `ClassOnly` answers a JDWP `MethodExit` request reported with the
    // forced return needs, as the suspend point computes them.
    let filter_classes = shared
        .debug
        .debug_state
        .lock()
        .events
        .location_filter_class_ids();
    let location_supers = if filter_classes.is_empty() {
        Vec::new()
    } else {
        let class_id = thread.frames.get(frame_idx)?.class_id;
        let cm = shared.classes.class_manager.read();
        super::debugger_supertypes_among(&cm, class_id, &filter_classes)
    };
    force_return_here(shared, thread, frame_idx, pc, tid, forced, &location_supers)
}

/// Return from frame `frame_idx` with the value [`force_return_here`] left
/// on its operand stack, as the decoded path's return arm returns
/// (`execute_frame_from_index`, `InstructionResult::Return`): the value is
/// narrowed to the return type and pushed onto the caller's operand stack
/// (across a continuation boundary when there is one), and the frame is
/// popped, releasing a `synchronized` method's monitor
/// (`pop_and_recycle_frame`). The block monitors the frame still holds are
/// released first, without `IllegalMonitorStateException`, as HotSpot
/// releases them (measured: `L1W43JdiForceEarlyReturn`'s `free = true`
/// after a forced return from inside a `synchronized` block). JVMTI
/// `MethodExit` fires as for a return opcode.
/// For the loop's entry frame the value is handed back instead
/// ([`ForcedReturnStep::Returned`]).
#[cold]
#[inline(never)]
pub(super) fn finish_forced_return(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    initial_frame_idx: usize,
    thread_is_virtual: bool,
) -> Result<ForcedReturnStep, crate::error::MethodCallFailed> {
    use crate::error::{MethodCallFailed, RuntimeError, VmError};
    let runtime = |e: RuntimeError| MethodCallFailed::InternalError(VmError::Runtime(e));
    let (released, refused) = release_forced_frame_block_monitors(shared, thread, frame_idx);
    if crate::runtime::env_cache::frame_trace() {
        if let Some(f) = thread.frames.get(frame_idx) {
            eprintln!(
                "[FORCED_RETURN] {}.{}{} entry_frame={} block_released={released} block_refused={refused} method_monitor={}",
                f.class_name(),
                f.method_name(),
                f.method_descriptor(),
                frame_idx <= initial_frame_idx,
                f.monitor_on_exit.is_some()
            );
        }
    }
    let Some(frame) = thread.frames.get_mut(frame_idx) else {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!("forced return: no frame {frame_idx}"),
        }));
    };
    let ret = frame.return_tag();
    let mut value = match ret {
        b'V' => None,
        b'J' => Some(Value::Long(frame.stack.pop_long().map_err(runtime)?)),
        b'F' => Some(Value::Float(frame.stack.pop_float().map_err(runtime)?)),
        b'D' => Some(Value::Double(frame.stack.pop_double().map_err(runtime)?)),
        b'L' | b'[' => Some(frame.stack.pop().map_err(runtime)?),
        _ => Some(Value::Int(frame.stack.pop_int().map_err(runtime)?)),
    };
    super::fire_method_exit_keeping_value(shared, thread, frame_idx, &mut value);
    if frame_idx <= initial_frame_idx {
        return Ok(ForcedReturnStep::Returned(
            value.map(|v| super::narrow_ireturn_value(v, ret)),
        ));
    }
    if let Some(f) = thread.frames.get_mut(frame_idx) {
        if f.locals_overlap_caller() {
            f.release_caller_overlap();
            super::invoke_phases::note_overlap_released();
        }
    }
    let value = value.map(|v| {
        if ret == b'V' {
            v
        } else {
            super::narrow_ireturn_value(crate::vm::coerce_value_for_return(v, ret), ret)
        }
    });
    if let Some(v) = value {
        if thread_is_virtual && !thread.continuation_return_adapters.is_empty() {
            super::push_return_across_continuation_boundary(shared, thread, frame_idx, v, None)?;
        } else {
            let parent = &mut thread.frames[frame_idx - 1];
            parent.exec_epoch = parent.exec_epoch.wrapping_add(1);
            super::field_access::push_invoke_return_value(&mut parent.stack, v).map_err(runtime)?;
        }
    } else if ret == b'V' && thread_is_virtual && !thread.continuation_return_adapters.is_empty()
    {
        // Interpreter round i6 wave 115, lane L4: a forced `void` return of a
        // remounted leaf still owes its method-handle door's `null`
        // (`CRATONVM_MH_VOID_TAIL_UNMOUNT`), as the `return` arms.
        super::push_void_return_across_continuation_boundary(thread, frame_idx)?;
    }
    super::pop_and_recycle_frame(shared, thread);
    Ok(ForcedReturnStep::Popped)
}

/// Pop thread frames `target..` (all of them above `target`, and `target`
/// itself) for a JDWP `StackFrame.PopFrames` (interpreter round i1 wave 44,
/// lane L1; `debug::pop_frames`, which checked every condition first and
/// read `args` from `target`'s parameter slots), on the parked thread
/// itself. Each frame is popped as the unwind pops one
/// ([`super::pop_and_recycle_frame`]): the block monitors it holds and a
/// `synchronized` method's monitor are released, as JDI specifies ("Locks
/// acquired by a popped frame are released when it is popped"), and no
/// method exit is reported ("No events are generated by this method"). The
/// caller then stands at its invoke (`pc` = `last_instr_pc`) with `args`
/// pushed back onto its operand stack in order (the receiver first), so the
/// dispatch loop re-runs the invoke when the thread resumes
/// ([`DebuggerStop::FramesPopped`]); a changed argument stays changed, as
/// JDI specifies. Answers how many frames were popped.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(crate) fn pop_frames_for_debugger(
    shared: &SharedVm,
    thread: &mut JvmThread,
    target: usize,
    args: &[Value],
) -> usize {
    // The arguments go onto the caller's operand stack FIRST, where they are
    // roots while the pops below run (a `FramePop` callback may run Java,
    // and so a collection); `debug::pop_frames`' checks refused a frame whose
    // locals overlap its caller's operand stack.
    let Some(caller_idx) = target.checked_sub(1) else {
        return 0;
    };
    if let Some(caller) = thread.frames.get_mut(caller_idx) {
        caller.pc = caller.last_instr_pc;
        for &arg in args {
            // Cannot fail: the invoke popped these very slots from the
            // caller's operand stack, which is no fuller now.
            let _ = match arg {
                Value::Long(v) => caller.stack.push_long(v),
                Value::Float(v) => caller.stack.push_float(v),
                Value::Double(v) => caller.stack.push_double(v),
                Value::Int(v) => caller.stack.push_int(v),
                v => caller.stack.push(v),
            };
        }
    }
    let mut popped = 0usize;
    while thread.frames.len() > target {
        // The block monitors as a forced return releases them (not through
        // the pop's pruning release; see `release_forced_frame_block_monitors`).
        let top = thread.frames.len() - 1;
        release_forced_frame_block_monitors(shared, thread, top);
        super::pop_and_recycle_frame(shared, thread);
        popped += 1;
    }
    if let Some(caller) = thread.frames.last_mut() {
        caller.exec_epoch = caller.exec_epoch.wrapping_add(1);
    }
    popped
}

/// Release, newest first, every block monitor frame `frame_idx` still
/// records, for a forced return (interpreter round i1 wave 44, lane L1, the
/// host's `free = false` on `L1W43JdiForceEarlyReturn`): each entry the
/// monitor table says this thread holds is released once, as the
/// `monitorexit` the forced return skips would release it. Answers
/// `(released, refused)`.
///
/// Not `held_monitors::release_all_recorded`, whose stale-entry pruning
/// (`prune_stale`) drops an entry WITHOUT releasing it whenever the table's
/// entry count for the object reads lower than every frame's records
/// together (`table_holds` answers 0 for an inflated monitor its index does
/// not name, and for an address `is_heap_addr` does not accept): a forced
/// return is the one removal of a frame in which the record is the only
/// account of the block monitors, so a dropped entry is a monitor held
/// forever. A refused entry is counted for the positive control, never
/// thrown: HotSpot releases a forced frame's monitors silently.
fn release_forced_frame_block_monitors(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
) -> (usize, usize) {
    let tid = thread.thread_id;
    let (mut released, mut refused) = (0usize, 0usize);
    while let Some(obj) = thread
        .frames
        .get_mut(frame_idx)
        .and_then(|f| f.held_monitors.pop())
    {
        let exited = shared.threads.monitors.holds(obj, tid)
            && crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, obj, tid).is_ok();
        if exited {
            released += 1;
        } else {
            refused += 1;
        }
    }
    (released, refused)
}

/// Delivery-side VM scoping for the interpreter's JVMTI event helpers.
///
/// The registry half of this (`runtime::jvmti::ENVIRONMENTS`) is pinned by
/// `runtime::jvmti`'s own tests. What is pinned *here* is the half those
/// cannot reach: that the interpreter helpers pass a real, exact
/// `vm_identity` down to the `fire_*_for_vm` family, rather than the
/// `UNATTRIBUTED_VM` migration seam they used before this change. A
/// regression that reverts any one helper to the VM-less `fire_*` free
/// function delivers VM A's MethodEntry to VM B's `-agentpath:` agent, which
/// a debugging interface people trust to be authoritative must never do.
///
/// Parallel safety: every test takes its own `scoped_vm()` identity from a
/// base no real `SharedVm` can reach (`NEXT_VM_IDENTITY` counts from 1), and
/// takes `jvmti_registry_test_lock()` because registering a listener moves
/// the process-wide **union** mirrors that `runtime::jvmti`'s tests assert
/// are false. Each test drops its rows again on the way out.
#[cfg(test)]
mod jvmti_delivery_scoping_tests {
    use super::*;
    use crate::runtime::jvmti::{
        self, EventCallbacks, EventMode, JvmtiEventKind, JvmtiEventManager, MethodId,
    };
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::{Arc, Mutex};

    /// A `vm_identity` no other test and no real VM can collide with. Real
    /// identities come from `NEXT_VM_IDENTITY` (`vm/src/vm/vm_init.rs:9`), a
    /// counter starting at 1, so small integers are NOT safe to fake with in a
    /// binary that also builds real `SharedVm`s. The base is distinct from
    /// `runtime::jvmti`'s own `scoped_test_vm()` base (`0x7000_0000`) so the
    /// two modules cannot hand out the same row even by accident. gcg g2/k:
    /// both now draw from the real counter (`vm_init::test_vm_identity`),
    /// which is unique against every real VM and every test by construction.
    fn scoped_vm() -> usize {
        crate::vm::vm_init::test_vm_identity()
    }

    /// Install an empty manager owned by `vm` and return it.
    fn manager_for(vm: usize) -> Arc<JvmtiEventManager> {
        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
        assert_eq!(
            mgr.vm_identity(),
            vm,
            "manager_for_vm must not resolve through the unattributed seam for an installed row"
        );
        mgr
    }

    /// Subscribe `mgr`'s VM to `kinds` and install `callbacks`.
    ///
    /// `set_event_callbacks` replaces the whole struct, so it is called once
    /// with every callback the test needs — calling it per kind would silently
    /// drop all but the last, which is exactly the shape of bug that makes a
    /// scoping test pass for the wrong reason.
    fn watch(mgr: &Arc<JvmtiEventManager>, kinds: &[JvmtiEventKind], callbacks: EventCallbacks) {
        for kind in kinds {
            mgr.set_event_notification_mode(EventMode::Enable, *kind, None)
                .expect("enabling an event on a fresh manager cannot fail");
        }
        mgr.set_event_callbacks(callbacks)
            .expect("installing callbacks on a fresh manager cannot fail");
    }

    type Seen = Arc<Mutex<Vec<(u64, MethodId)>>>;

    fn seen() -> Seen {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn method_entry_cb(sink: &Seen) -> EventCallbacks {
        let s = sink.clone();
        EventCallbacks {
            method_entry: Some(Box::new(move |t, m| s.lock().unwrap().push((t, m)))),
            ..Default::default()
        }
    }

    fn method_exit_cb(sink: &Seen) -> EventCallbacks {
        let s = sink.clone();
        EventCallbacks {
            method_exit: Some(Box::new(move |t, m, _exc, _rv| {
                s.lock().unwrap().push((t, m))
            })),
            ..Default::default()
        }
    }

    fn test_frame(method_name: &str) -> Frame {
        Frame::new(
            ClassId::new(0),
            "T".to_string(),
            method_name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    fn test_thread(name: &str) -> JvmThread {
        JvmThread::new(ThreadId(1234), name)
    }

    /// Two overloads of one name must not share an event method id. The
    /// synthesized fallback hashed the name only, so `m()V` and `m(I)V` were
    /// indistinguishable to an agent.
    #[test]
    fn overloads_get_distinct_synthesized_method_ids() {
        let overload = |descriptor: &str| {
            Frame::new(
                ClassId::new(7),
                "T".to_string(),
                "m".to_string(),
                descriptor.to_string(),
                None,
                vec![0xb1],
                vec![],
                8,
                4,
                &[],
            )
        };
        let (a, b) = (overload("()V"), overload("(I)V"));
        assert_ne!(synth_method_id(&a), synth_method_id(&b));
        // The class id stays in the upper half.
        assert_eq!(synth_method_id(&a) >> 32, 7);
        // A VM that cannot be reached (no bridge) falls back to the synthesized id.
        let vm = scoped_vm();
        assert_eq!(jvmti_method_id(vm, &a), synth_method_id(&a));
    }

    /// MethodExit raised with VM A's identity reaches A's agent and never B's.
    #[test]
    fn method_exit_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        watch(&ma, &[JvmtiEventKind::MethodExit], method_exit_cb(&sa));
        watch(&mb, &[JvmtiEventKind::MethodExit], method_exit_cb(&sb));

        let mut thread = test_thread("method-exit-scoping");
        thread.frames.push(test_frame("m"));
        let expected = synth_method_id(&thread.frames[0]);
        fire_jvmti_method_exit_normal(a, &mut thread, 0, &Some(Value::Int(7)));

        assert_eq!(
            *sa.lock().unwrap(),
            vec![(thread.thread_id.0, expected)],
            "the raising VM's agent must see exactly one MethodExit"
        );
        assert!(
            sb.lock().unwrap().is_empty(),
            "a second VM's agent must never see another VM's MethodExit"
        );

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// `push_frame_and_fire_entry` is the single frame-push chokepoint, so a
    /// missed identity there mis-attributes *every* MethodEntry in the VM.
    #[test]
    fn push_frame_and_fire_entry_attributes_method_entry_to_its_vm() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        watch(&ma, &[JvmtiEventKind::MethodEntry], method_entry_cb(&sa));
        watch(&mb, &[JvmtiEventKind::MethodEntry], method_entry_cb(&sb));

        let mut thread = test_thread("method-entry-scoping");
        let frame = test_frame("entered");
        let expected = synth_method_id(&frame);
        push_frame_and_fire_entry(b, &mut thread, frame);

        assert_eq!(thread.frames.len(), 1, "the frame must still be pushed");
        assert_eq!(
            *sb.lock().unwrap(),
            vec![(thread.thread_id.0, expected)],
            "the pushing VM's agent must see the MethodEntry"
        );
        assert!(
            sa.lock().unwrap().is_empty(),
            "the other VM's agent must not see it"
        );

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// FramePop consumes the request on the raising VM's thread and delivers
    /// to that VM only.
    #[test]
    fn frame_pop_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::FramePop],
            EventCallbacks {
                frame_pop: Some(Box::new(move |t, m, _exc| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::FramePop],
            EventCallbacks {
                frame_pop: Some(Box::new(move |t, m, _exc| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let mut thread = test_thread("frame-pop-scoping");
        thread.frames.push(test_frame("popping"));
        let expected = synth_method_id(&thread.frames[0]);
        thread.frame_pop_requests.push(0);
        fire_jvmti_frame_pop_if_requested(a, &mut thread, false);

        assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
        assert!(sb.lock().unwrap().is_empty());
        assert!(
            thread.frame_pop_requests.is_empty(),
            "NotifyFramePop is one-shot: the matching request must be consumed"
        );

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// Wave 10 (L4): a `NotifyFramePop` request is consumed when its frame
    /// pops even if no FramePop listener is active then, so it cannot match a
    /// later, unrelated frame at the same depth. A request for another depth
    /// is kept.
    #[test]
    fn a_frame_pop_request_dies_with_its_frame_without_a_listener() {
        let _lock = jvmti::jvmti_registry_test_lock();
        // No manager is installed for this VM: nothing can be delivered.
        let vm = scoped_vm();
        let mut thread = test_thread("frame-pop-no-listener");
        thread.frames.push(test_frame("outer"));
        thread.frames.push(test_frame("popping"));
        thread.frame_pop_requests.push(0);
        thread.frame_pop_requests.push(1);
        fire_jvmti_frame_pop_if_requested(vm, &mut thread, false);
        assert_eq!(
            thread.frame_pop_requests,
            vec![0],
            "the popping frame's request must be consumed, the outer one kept"
        );
    }

    /// SingleStep reaches only the raising VM's agent, and within it only a
    /// thread the event is enabled for: globally, or (wave 22) through the
    /// manager's per-thread enable, which is what JVMTI's per-thread
    /// `SetEventNotificationMode` sets. (The per-thread gate used to be
    /// `JvmThread::single_step_enabled`, which production never raised.)
    #[test]
    fn single_step_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::SingleStep],
            EventCallbacks {
                single_step: Some(Box::new(move |t, m, _loc| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::SingleStep],
            EventCallbacks {
                single_step: Some(Box::new(move |t, m, _loc| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let mut thread = test_thread("single-step-scoping");
        thread.frames.push(test_frame("stepped"));
        let expected = synth_method_id(&thread.frames[0]);

        // Enabled globally on VM a: its agent hears it, VM b's does not,
        // with no per-thread flag raised anywhere.
        fire_jvmti_single_step(a, &mut thread, 0, 3);
        assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
        assert!(sb.lock().unwrap().is_empty());

        // Enabled for ANOTHER thread only: this one's steps reach nobody.
        sa.lock().unwrap().clear();
        ma.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::SingleStep, None)
            .expect("disable");
        ma.set_event_notification_mode(
            EventMode::Enable,
            JvmtiEventKind::SingleStep,
            Some(thread.thread_id.0 + 1),
        )
        .expect("enable for one thread");
        fire_jvmti_single_step(a, &mut thread, 0, 4);
        assert!(
            sa.lock().unwrap().is_empty(),
            "the per-thread enable must be honoured"
        );
        // ...and for this thread: it does.
        ma.set_event_notification_mode(
            EventMode::Enable,
            JvmtiEventKind::SingleStep,
            Some(thread.thread_id.0),
        )
        .expect("enable for this thread");
        fire_jvmti_single_step(a, &mut thread, 0, 5);
        assert_eq!(*sa.lock().unwrap(), vec![(thread.thread_id.0, expected)]);
        assert!(sb.lock().unwrap().is_empty());

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// ExceptionCatch is the one helper that has only a `&Frame` — no thread,
    /// no VM — so it is the likeliest to be reverted to the VM-less fire.
    #[test]
    fn exception_catch_reaches_only_the_raising_vms_agent() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::ExceptionCatch],
            EventCallbacks {
                exception_catch: Some(Box::new(move |t, m, _loc, _exc| {
                    ca.lock().unwrap().push((t, m))
                })),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::ExceptionCatch],
            EventCallbacks {
                exception_catch: Some(Box::new(move |t, m, _loc, _exc| {
                    cb.lock().unwrap().push((t, m))
                })),
                ..Default::default()
            },
        );

        let catching_thread = 4321u64;
        let mut thread = JvmThread::new(ThreadId(catching_thread), "catcher");
        thread.frames.push(test_frame("catcher"));
        let expected = synth_method_id(&thread.frames[0]);
        fire_jvmti_exception_catch(b, &mut thread, 0, 17);

        assert_eq!(
            *sb.lock().unwrap(),
            vec![(catching_thread, expected)],
            "ExceptionCatch carries the catching thread's id (it used to be a constant 0), \
             and the VM must be exact"
        );
        assert!(sa.lock().unwrap().is_empty());

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }

    /// Wave 12: the catch hook resolves the method id only when the VM has
    /// `ExceptionCatch` enabled for the thread, not whenever any agent
    /// listens for anything (the union guard): an agent that enabled only
    /// `Breakpoint` raises the union and must not make every caught
    /// exception pay for the id.
    #[test]
    fn exception_catch_is_resolved_only_when_enabled() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let vm = scoped_vm();
        let mgr = manager_for(vm);
        let seen = seen();
        let sink = seen.clone();
        watch(
            &mgr,
            &[JvmtiEventKind::Breakpoint],
            EventCallbacks {
                exception_catch: Some(Box::new(move |t, m, _loc, _exc| {
                    sink.lock().unwrap().push((t, m))
                })),
                ..Default::default()
            },
        );
        assert!(jvmti::any_listener_active(), "the union guard is up");
        assert!(!jvmti::exception_catch_enabled_for_vm(vm, 7));
        let mut thread = JvmThread::new(ThreadId(7), "catcher");
        thread.frames.push(test_frame("catcher"));
        fire_jvmti_exception_catch(vm, &mut thread, 0, 3);
        assert!(seen.lock().unwrap().is_empty());
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::ExceptionCatch, None)
            .expect("enable ExceptionCatch");
        assert!(jvmti::exception_catch_enabled_for_vm(vm, 7));
        fire_jvmti_exception_catch(vm, &mut thread, 0, 3);
        assert_eq!(*seen.lock().unwrap(), vec![(7, synth_method_id(&thread.frames[0]))]);
        jvmti::forget_vm_jvmti_state(vm);
    }

    /// The load-bearing asymmetry: the `any_*_listener_active()` guards are a
    /// process-wide **union**, so a VM with no agent at all still enters the
    /// helper body when some *other* VM is listening. Delivery must then find
    /// nothing. If a helper trusted the union flag instead of re-resolving the
    /// row, this is the test that fails — and the bug it would be hiding is an
    /// event delivered to an agent that never asked for it.
    #[test]
    fn a_union_guard_never_delivers_another_vms_event() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (quiet, loud) = (scoped_vm(), scoped_vm());
        let _quiet_mgr = manager_for(quiet); // installed, but subscribes to nothing
        let loud_mgr = manager_for(loud);
        let heard = seen();
        let (entry_sink, exit_sink) = (heard.clone(), heard.clone());
        watch(
            &loud_mgr,
            &[JvmtiEventKind::MethodEntry, JvmtiEventKind::MethodExit],
            EventCallbacks {
                method_entry: Some(Box::new(move |t, m| {
                    entry_sink.lock().unwrap().push((t, m))
                })),
                method_exit: Some(Box::new(move |t, m, _exc, _rv| {
                    exit_sink.lock().unwrap().push((t, m))
                })),
                ..Default::default()
            },
        );

        // The union is true because `loud` is listening — that is the whole
        // point of the guard, and it is what puts `quiet`'s interpreter on the
        // slow path.
        assert!(
            jvmti::any_method_entry_listener_active(),
            "the union guard must be set while any VM listens"
        );
        assert!(
            !jvmti::any_method_entry_listener_active_for_vm(quiet),
            "the exact per-VM query must disagree with the union here"
        );

        let mut thread = test_thread("union-guard");
        thread.frames.push(test_frame("m"));
        fire_jvmti_method_exit_normal(quiet, &mut thread, 0, &None);
        push_frame_and_fire_entry(quiet, &mut thread, test_frame("m"));

        assert!(
            heard.lock().unwrap().is_empty(),
            "an over-approximating guard must not turn into an over-approximating delivery"
        );

        jvmti::forget_vm_jvmti_state(quiet);
        jvmti::forget_vm_jvmti_state(loud);
    }

    /// Field watchpoints: the four getfield/getstatic/putfield/putstatic sites
    /// pass `shared.vm_identity`, so a watch armed in one VM must not fire on
    /// the same `(class_id, field_index)` in another. `class_id` is only
    /// unique *within* a VM, so this pair genuinely aliases.
    #[test]
    fn field_watchpoints_do_not_alias_across_vms() {
        let _lock = jvmti::jvmti_registry_test_lock();
        let (a, b) = (scoped_vm(), scoped_vm());
        let (ma, mb) = (manager_for(a), manager_for(b));
        let (sa, sb) = (seen(), seen());
        let ca = sa.clone();
        let cb = sb.clone();
        watch(
            &ma,
            &[JvmtiEventKind::FieldAccess],
            EventCallbacks {
                field_access: Some(Box::new(move |t, m, _f| ca.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );
        watch(
            &mb,
            &[JvmtiEventKind::FieldAccess],
            EventCallbacks {
                field_access: Some(Box::new(move |t, m, _f| cb.lock().unwrap().push((t, m)))),
                ..Default::default()
            },
        );

        let (class_id, field_index) = (0xDEAD_BEEF_u64, 3usize);
        jvmti::set_field_watchpoint_for_vm(a, class_id, field_index, true, false)
            .expect("arming a watchpoint on a fresh row cannot fail");

        // Exactly the call the getstatic/getfield sites now make.
        jvmti::fire_field_access_if_watched_for_vm(b, 1234, 0x99, class_id, field_index);
        assert!(
            sb.lock().unwrap().is_empty(),
            "VM B has no watch on this (class_id, field_index) — B's ids mean different classes"
        );
        assert!(
            sa.lock().unwrap().is_empty(),
            "and B's access must certainly not be reported to A, which does watch it"
        );

        jvmti::fire_field_access_if_watched_for_vm(a, 1234, 0x99, class_id, field_index);
        assert_eq!(
            sa.lock().unwrap().len(),
            1,
            "A's own access must reach A's agent"
        );
        assert!(sb.lock().unwrap().is_empty());

        jvmti::forget_vm_jvmti_state(a);
        jvmti::forget_vm_jvmti_state(b);
    }
}

/// Interpreter round i1 wave 12, lane L4: the verdict a compiled back-edge
/// poll's slow path returns, and the request that makes the polls ask it.
#[cfg(test)]
mod gci_i11ja_poster_thread_tests {
    //! gci i11/ja (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    //! item 1, part 2): a poster installs its own handle around the delivery
    //! and puts the previous binding back, also on unwind.
    use super::*;
    use crate::native::jni::{installed_jni_thread_addr, replace_jni_thread, restore_jni_thread};
    use crate::threading::jvm_thread::ThreadId;

    #[test]
    fn gci_i11ja_a_post_runs_with_the_posters_thread_installed() {
        std::thread::spawn(|| {
            let mut poster = JvmThread::new(ThreadId(7), "poster");
            let own = &mut poster as *mut JvmThread as usize;
            assert_eq!(installed_jni_thread_addr(), None, "a fresh OS thread");
            let seen = post_on_own_thread(&mut poster, installed_jni_thread_addr);
            assert_eq!(seen, Some(own), "the poster's own handle, during the post");
            assert_eq!(installed_jni_thread_addr(), None, "nothing left behind");
            // Under an outer binding (a native funnel's): put back exactly.
            let mut outer = JvmThread::new(ThreadId(8), "outer");
            let outer_addr = &mut outer as *mut JvmThread as usize;
            // SAFETY: `outer` lives until the restore below.
            let prev = unsafe { replace_jni_thread(&mut outer as *mut JvmThread) };
            let seen = post_on_own_thread(&mut poster, installed_jni_thread_addr);
            assert_eq!(seen, Some(own));
            assert_eq!(installed_jni_thread_addr(), Some(outer_addr), "the outer binding");
            // An unwinding post restores too.
            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                post_on_own_thread(&mut poster, || -> u8 { panic!("simulated callback unwind") })
            }));
            assert!(unwound.is_err());
            assert_eq!(installed_jni_thread_addr(), Some(outer_addr), "restored on unwind");
            // SAFETY: what this thread had installed before `outer`.
            unsafe { restore_jni_thread(prev) };
            assert_eq!(installed_jni_thread_addr(), None);
        })
        .join()
        .expect("poster thread");
    }

    /// Source pin: every poster in this file that holds the thread's `&mut`
    /// delivers through the helper; the two that also hold a
    /// `NativeFrameRow` share the row's pointer.
    #[test]
    fn gci_i11ja_mut_posters_install_their_own_handle() {
        let src = include_str!("jvmti_events.rs").replace("\r\n", "\n");
        let item = |name: &str| -> String {
            let start = src.find(name).unwrap_or_else(|| panic!("{name}"));
            let rest = &src[start..];
            rest[..rest.find("\n}\n").expect("its end")].to_string()
        };
        for f in [
            "pub(super) fn fire_jvmti_frame_pop_if_requested(",
            // The body both entry hooks share since origin/dev's wave 95
            // (`fire_method_entry_after_push` / `_in_loop` delegate to it).
            "\nfn fire_method_entry_after_push_with(",
            "\nfn fire_method_entry_at_callback_point(",
            "pub(crate) fn fire_deferred_method_entry(",
        ] {
            let b = item(f);
            assert!(b.contains("post_on_own_thread(thread, || {"), "{f}");
            let post = b.find("post_on_own_thread(").expect("post");
            let fire = b.find("crate::runtime::jvmti::fire_").expect("fire");
            assert!(post < fire, "{f}: the fire is inside the post");
        }
        let entry = item("fn report_named_native_method_entry(");
        assert!(entry.contains("enter_hashed(shared, own, declaring, method_id)"));
        assert!(entry.contains("post_on_own_thread_ptr(own, || {"));
        let exit = item("pub(crate) fn report_native_method_exit(");
        assert!(exit.contains("post_on_own_thread_ptr(own, || {"));
        assert!(exit.contains("post_on_own_thread(thread, || {"));
        assert!(!exit.contains("thread as *mut JvmThread,\n"), "the row takes `own`");
    }

    /// gci i11/ja item 3 (source pin; `jni.rs` lives outside this module): JNI
    /// `MonitorEnter`'s unbound arm makes no `&mut` of the registry's
    /// `JvmThread` for a caller already in its blocked region, which acquires
    /// as an excluded thread; only the counted case still takes it.
    #[test]
    fn gci_i11ja_unbound_monitor_enter_spares_a_blocked_caller_the_mut() {
        let src = include_str!("../../native/jni.rs").replace("\r\n", "\n");
        let start = src.find("extern \"C\" fn jni_monitor_enter(").expect("MonitorEnter");
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let arm = body.find("if let Some(thread) = unbound_caller_jvm_thread(shared)").expect("arm");
        let arm = &body[arm..];
        let view = arm.find("&*(thread as *const JvmThread)").expect("a shared view");
        let excluded = arm.find("jni_monitor_enter_excluded(shared, obj, oref, me)?;").expect("excluded");
        let through = arm
            .find("jni_monitor_enter_through_downcall(shared, me, obj, oref)")
            .expect("the downcall's own binding first");
        let counted = arm.find("jni_monitor_enter_as(shared, unsafe { &mut *thread }, obj, oref)?;").expect("counted");
        assert!(view < excluded && excluded < through && through < counted);
        assert_eq!(arm[..counted].matches("&mut *thread").count(), 0, "no `&mut` before the fallback");
        // The helper borrows through the binding the downcall's context installs.
        let start = src.find("\nfn jni_monitor_enter_through_downcall(").expect("helper");
        let rest = &src[start + 1..];
        let helper = &rest[..rest.find("\n}\n").expect("its end")];
        let ctx = helper.find("panama_libffi::with_active_context(").expect("the downcall context");
        let binding = helper.find("ctx.run_with_jni_binding(").expect("its binding");
        let bound = helper.find("with_jni_context(").expect("the bound thread");
        assert!(ctx < binding && binding < bound);
        assert!(helper.contains("ctx.vm_identity() != vm"), "this VM's downcall only");
        assert!(!helper.contains("unsafe"), "no raw re-derive in the helper");
    }

    /// gci i11/ja item 1b (source pin): every C-agent delivery
    /// (`crate::runtime::jvmti::fire_*`) in the interpreter's posting files
    /// runs under `post_on_own_thread` / `post_on_own_thread_ptr`.
    #[test]
    fn gci_i11ja_every_interpreter_poster_installs_its_own_handle() {
        let files = [
            ("jvmti_events.rs", include_str!("jvmti_events.rs")),
            ("../interpreter.rs", include_str!("../interpreter.rs")),
            ("opcodes.rs", include_str!("opcodes.rs")),
            ("lambda.rs", include_str!("lambda.rs")),
            ("exception_dispatch.rs", include_str!("exception_dispatch.rs")),
        ];
        let needle = "crate::runtime::jvmti::fire_";
        let mut seen = 0;
        for (name, raw) in files {
            let src = raw.replace("\r\n", "\n");
            // This file's test modules quote the needle: production only.
            let src = if name == "jvmti_events.rs" {
                let cut = src.find("\n#[cfg(test)]\n").expect("tests");
                src[..cut].to_string()
            } else {
                src
            };
            let mut from = 0;
            while let Some(at) = src[from..].find(needle) {
                let at = from + at;
                // gci i12/ev: the files hold non-ASCII text; start the
                // window on a char boundary (a mid-char slice panics).
                let mut lo = at.saturating_sub(400);
                while !src.is_char_boundary(lo) {
                    lo += 1;
                }
                let window = &src[lo..at];
                assert!(
                    window.contains("post_on_own_thread"),
                    "{name}: a delivery outside the poster's own handle at byte {at}"
                );
                seen += 1;
                from = at + needle.len();
            }
        }
        assert!(seen >= 20, "the posters were found ({seen})");
    }

    /// gci i11/ja item 3: the VM's `run_with_jni_binding` installs the
    /// context's own thread for the closure and restores the previous
    /// binding (nothing) afterwards.
    #[test]
    fn gci_i11ja_run_with_jni_binding_installs_the_contexts_thread() {
        use cratonvm_native_api::NativeThreadAccess;
        std::thread::spawn(|| {
            let shared = std::sync::Arc::new(SharedVm::new(crate::config::VmConfig::default()));
            *shared.self_arc.write() = Some(std::sync::Arc::downgrade(&shared));
            let mut thread = JvmThread::new(ThreadId(9), "downcall");
            let own = &mut thread as *mut JvmThread as usize;
            let mut seen = None;
            {
                let mut ctx = crate::vm::NativeContextImpl {
                    shared: &shared,
                    thread: &mut thread,
                };
                ctx.run_with_jni_binding(&mut || seen = installed_jni_thread_addr());
            }
            assert_eq!(seen, Some(own), "the context's thread, during the closure");
            assert_eq!(installed_jni_thread_addr(), None, "the previous binding, after");
        })
        .join()
        .expect("binding thread");
    }
}

#[cfg(test)]
mod gci_i12ev_poster_thread_tests {
    //! gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    //! item 1c), source pins (the posters are `experimental-debug` code; these
    //! read the files, so they run in every build): the thread start / end,
    //! class prepare, lambda-proxy class and allocation posters deliver to C
    //! agents under `post_on_own_thread` / `post_on_own_thread_ptr`, and
    //! the thread-bearing class load runs under it while a C env listens for
    //! `ClassLoad`.

    fn normalized(raw: &str) -> String {
        raw.replace("\r\n", "\n")
    }

    /// The text of the item starting at `start` up to its closing `end`.
    fn item<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        let at = src.find(start).unwrap_or_else(|| panic!("{start}"));
        let rest = &src[at..];
        &rest[..rest.find(end).unwrap_or_else(|| panic!("the end of {start}"))]
    }

    /// Every occurrence of `call` in `src` sits inside a `wrap` opened at
    /// most `reach` bytes before it; answers how many there are.
    fn each_wrapped(src: &str, call: &str, wrap: &str, reach: usize) -> usize {
        let mut seen = 0;
        let mut from = 0;
        while let Some(at) = src[from..].find(call) {
            let at = from + at;
            // The files hold non-ASCII text: start the window on a char.
            let mut lo = at.saturating_sub(reach);
            while !src.is_char_boundary(lo) {
                lo += 1;
            }
            let window = &src[lo..at];
            assert!(window.contains(wrap), "`{call}` outside `{wrap}` at byte {at}");
            seen += 1;
            from = at + call.len();
        }
        seen
    }

    #[test]
    fn gci_i12ev_thread_start_and_end_post_on_the_workers_own_handle() {
        let src = normalized(include_str!("../../vm/vm_exec.rs"));
        let posts = each_wrapped(
            &src,
            "crate::jvmti::native_env::post_thread_event(&shared_arc,",
            "crate::runtime::interpreter::post_on_own_thread(&mut jvm_thread, || {",
            120,
        );
        assert_eq!(posts, 2, "ThreadStart and ThreadEnd");
        // A virtual thread's start / mount / unmount / end, on its own handle
        // (`resume_virtual_continuation`).
        let wrap = "crate::runtime::interpreter::post_on_own_thread(&mut thread, || {";
        let posts = each_wrapped(&src, "crate::jvmti::native_env::post_virtual_thread(", wrap, 120);
        assert_eq!(posts, 2, "a resumed mount, a park's unmount");
        let posts = each_wrapped(&src, "post_virtual_thread(&shared, tid.0, VirtualThreadEvent::", wrap, 220);
        assert_eq!(posts, 4, "start + mount, unmount + end");
        // `VirtualThread.notifyJvmti*`'s native (`jdk_continuation.rs`).
        let cont = normalized(include_str!("../../vm/jdk_continuation.rs"));
        let body = item(&cont, "pub(crate) fn post_transition(", "\n}\n");
        let wrap = body
            .find("crate::runtime::interpreter::post_on_own_thread(ctx.thread, || match kind {")
            .expect("the wrap");
        let first = body.find("post_virtual_thread(&shared_arc,").expect("a post");
        assert!(wrap < first);
    }

    #[test]
    fn gci_i12ev_class_prepare_posts_on_the_preparing_threads_handle() {
        let src = normalized(include_str!("../../vm/vm_util.rs"));
        let body = item(&src, "\nfn link_claimed_class(", "\n}\n");
        let wrap = body
            .find("crate::runtime::interpreter::post_on_own_thread(thread, || {")
            .expect("the wrap");
        let post = body
            .find("crate::debug::class_prepared_on_thread(shared, tid, class_id)")
            .expect("the post");
        assert!(wrap < post && post - wrap < 320, "the post is inside the wrap");
        // gci i12/ev second pass: deferred `ClassLoad`s first, inside it.
        let flush = body
            .find("crate::jvmti::native_env::flush_deferred_class_loads(shared, tid);")
            .expect("the flush");
        assert!(wrap < flush && flush < post);
        assert_eq!(body.matches("class_prepared_on_thread(").count(), 1, "one post");
        // The lambda proxy's class events, in `bootstrap_lambda`.
        let src = normalized(include_str!("../invokedynamic.rs"));
        let posts = each_wrapped(
            &src,
            "crate::debug::lambda_proxy_minted(",
            "crate::runtime::interpreter::post_on_own_thread(thread, || {",
            120,
        );
        assert_eq!(posts, 1, "the proxy's ClassPrepare / ClassLoad");
    }

    #[test]
    fn gci_i12ev_a_thread_bearing_class_load_installs_its_handle_for_class_load_agents() {
        let src = normalized(include_str!("../../vm/vm_init.rs"));
        let body = item(&src, "pub fn load_class_transformed(", "\n    }\n");
        let gate = body.find("debugger_gates.native_class_load_armed()").expect("the gate");
        let wrap = body
            .find("crate::runtime::interpreter::post_on_own_thread(thread, || {")
            .expect("the wrap");
        let load = body.find("self.load_class_concurrent(name)").expect("the load");
        assert!(gate < wrap && wrap < load, "gate, wrap, then the load inside it");
        assert!(
            body.ends_with("self.load_class_concurrent(name)"),
            "no listener: the plain load"
        );
        assert!(body.contains("#[cfg(feature = \"experimental-debug\")]"));
        // The define natives' single backend: the define (whose guard drop
        // posts `ClassLoad`) touches `shared` only, and runs under the wrap.
        let src = normalized(include_str!("../../vm/vm_exec.rs"));
        let body = item(&src, "    fn define_class_full(", "\n    }\n");
        // gci i21/fx: the 2026-10-15 merge brought dev's lane superAcc, whose
        // closure returns the raw `VmError` so the unnamed-module hash (which
        // needs `self`) is appended after it -- the define still reaches no
        // `self`, and the refusal is named outside the wrap.
        let open = body
            .find("let define = || -> Result<(ClassId, ClassId), VmError> \u{7b}")
            .expect("the define closure");
        let close = body.find("Ok((cid, next_id_before))").expect("its end");
        assert!(open < close);
        assert!(!body[open..close].contains("self."), "the define reaches no `self`");
        let gate = body.find("debugger_gates.native_class_load_armed()").expect("the gate");
        let wrap = body
            .find("crate::runtime::interpreter::post_on_own_thread(self.thread, define)")
            .expect("the wrap");
        assert!(close < gate && gate < wrap);
        let taken = body
            .find("let (cid, next_id_before) = match defined \u{7b}")
            .expect("the define's answer taken after the wrap");
        let named = body
            .find("self.define_refusal_with_unnamed_module_hash(e, cl_id)")
            .expect("a refusal named outside the closure");
        assert!(wrap < taken && taken < named);
    }

    #[test]
    fn gci_i12ev_allocation_events_install_the_allocating_threads_pointer() {
        let src = normalized(include_str!("../../jvmti/native_env/events_more.rs"));
        let body = item(&src, "\nfn post_alloc(", "\n}\n");
        let ptr = body.find("let thread_ptr: *mut JvmThread = thread;").expect("one pointer");
        let deliver = body.find("let deliver = || {").expect("the deliveries");
        let post = body
            .find("crate::runtime::interpreter::post_on_own_thread_ptr(thread_ptr, deliver)")
            .expect("installed around them");
        assert!(ptr < deliver && deliver < post);
        assert_eq!(body.matches("&mut *thread_ptr").count(), 1, "alloc_facts' borrow only");
        // `ResourceExhausted`, posted where the VM builds an OOME.
        let src = normalized(include_str!("../exceptions.rs"));
        let posts = each_wrapped(
            &src,
            "crate::jvmti::native_env::post_resource_exhausted(",
            "crate::runtime::interpreter::post_on_own_thread(thread, || {",
            120,
        );
        assert_eq!(posts, 1);
    }

    /// gci i12/ev second pass: the deferred-`ClassLoad` flush points
    /// (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`) and the `NativeMethodBind` /
    /// `ClassFileLoadHook` posters.
    #[test]
    fn gci_i12ev_flush_points_and_the_bind_and_hook_posters_use_the_own_handle() {
        let exec = normalized(include_str!("../../vm/vm_exec.rs"));
        // The native funnel's binding: flush once the own handle is installed.
        let install = item(&exec, "    unsafe fn install(shared: &SharedVm, thread: *mut JvmThread) -> Self {", "\n    }\n");
        let bound = install.find("replace_jni_thread(thread)").expect("the binding");
        let flush = install
            .find("crate::jvmti::native_env::flush_deferred_class_loads(shared, tid);")
            .expect("the flush");
        assert!(bound < flush);
        // The platform and the virtual thread's last own-handle points.
        let flushes = each_wrapped(
            &exec,
            "crate::jvmti::native_env::flush_deferred_class_loads(&shared",
            "crate::runtime::interpreter::post_on_own_thread(&mut ",
            120,
        );
        assert_eq!(flushes, 2, "thread end, virtual thread end");
        // The three compatibility define doors define on the own handle, and
        // their defines reach no `self`.
        for door in [
            "    fn define_class_from_bytes(",
            "    fn define_hidden_class_from_bytes(",
            "    fn define_class_with_loader(",
        ] {
            let body = item(&exec, door, "\n    }\n");
            let opener = "define_on_own_handle(shared, self.thread, || {";
            let wrap = body.find(opener).unwrap_or_else(|| panic!("{door}"));
            assert!(
                !body[wrap + opener.len()..].contains("self."),
                "{door}: the define reaches no `self`"
            );
        }
        let helper = item(&exec, "\nfn define_on_own_handle<R>(", "\n}\n");
        assert!(helper.contains("native_class_load_armed()"));
        assert!(helper.contains("crate::runtime::interpreter::post_on_own_thread(thread, define)"));
        // `post_rust_native_method_bind` posts under the funnel's handle.
        let more = normalized(include_str!("../../jvmti/native_env/events_more.rs"));
        let bind = item(&more, "pub(crate) fn post_rust_native_method_bind(", "\n}\n");
        // gci i21/fx: the signature compared with its whitespace removed --
        // rustfmt splits it one parameter per line once it outgrows a line,
        // which is the layout the file has now; the parameters are the pin.
        let sig: String = bind[..bind.find(") -> bool").expect("the signature")]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(
            sig.starts_with(
                "pub(crate)fnpost_rust_native_method_bind(shared:&SharedVm,thread:&mutJvmThread,"
            ),
            "{sig}"
        );
        let wrap = bind
            .find("crate::runtime::interpreter::post_on_own_thread(thread, || {")
            .expect("the wrap");
        let post = bind.find("post_native_method_bind(shared, tid,").expect("the post");
        assert!(wrap < post);
        // The C `ClassFileLoadHook` offer runs inside the context's binding.
        let inst = normalized(include_str!("../instrument.rs"));
        let binding = inst.find("ctx.run_with_jni_binding(&mut || {").expect("the binding");
        let offer = inst
            .find("answer = post_native_class_file_load_hook(")
            .expect("the offer");
        assert!(binding < offer && offer - binding < 120);
    }
}

#[cfg(test)]
mod i12_l4_loop_exit_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    fn frame_of(method_name: &str) -> Frame {
        Frame::new(
            ClassId::new(0),
            "T".to_string(),
            method_name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    /// The verdict is the OSR gate's own question about the innermost frame:
    /// an interpreter-only JVMTI event in this VM turns it on, and turning the
    /// event off turns it off again. No frame, no verdict. And with no OSR
    /// body running in the VM, the request pauses nothing.
    #[test]
    fn the_loop_exit_verdict_follows_the_interpreter_only_mode() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            let mut thread = JvmThread::new(ThreadId(0), "loop");
            assert!(
                !compiled_loop_must_leave(&shared, &thread),
                "no frame: nothing to leave"
            );
            thread.frames.push(frame_of("spin"));
            assert!(
                !compiled_loop_must_leave(&shared, &thread),
                "no agent: the loop stays compiled"
            );

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            assert!(
                compiled_loop_must_leave(&shared, &thread),
                "MethodEntry is posted by the interpreter only: the loop must leave"
            );
            // Wave 15: the mode concerns every compiled frame, so the request
            // no longer depends on a running OSR body; a fixture built
            // without `Vm::new` still has no owning handle to pause with.
            assert!(
                !request_compiled_loop_exits(&shared),
                "a unit fixture has no owning handle, so no pause is taken"
            );

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            assert!(
                !compiled_loop_must_leave(&shared, &thread),
                "the mode is off again: stay compiled"
            );
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// Wave 13: the loop-exit request repeats its pause only when the last one
    /// froze a peer (which passed no poll) while an OSR body still runs, and
    /// its total grace stays a few milliseconds.
    #[test]
    fn the_loop_exit_pause_is_repeated_only_for_a_frozen_peer_of_a_running_body() {
        assert!(loop_exit_pause_again(true, true));
        assert!(!loop_exit_pause_again(true, false), "the loop left");
        assert!(
            !loop_exit_pause_again(false, true),
            "every poll was reached"
        );
        assert!(!loop_exit_pause_again(false, false));
        let mut total = std::time::Duration::ZERO;
        let mut grace = LOOP_EXIT_GRACE;
        for _ in 0..LOOP_EXIT_ATTEMPTS {
            total += grace;
            grace = grace.saturating_mul(2);
        }
        assert!(
            total <= std::time::Duration::from_millis(20),
            "the grace slices of one request stay bounded: {total:?}"
        );
    }
}

/// Interpreter round i1 wave 15, lane L3
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, stage 3): the
/// every-frame verdict bit a method-entry body leaves on, and the sinks that
/// take such an exit uncharged.
#[cfg(test)]
mod i15_l3_method_entry_exit_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{DeoptCause, DeoptReason, ReconstructedFrame, ResumeSemantics};
    use std::sync::Arc;

    fn frame_of(method_name: &str) -> Frame {
        Frame::new(
            ClassId::new(0),
            "T".to_string(),
            method_name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    fn stashed(method_key: &str) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: method_key.to_string(),
            bci: 7,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        }
    }

    fn cause(reason: DeoptReason) -> Option<DeoptCause> {
        Some(DeoptCause {
            reason,
            speculation_id: cratonvm_jit::deopt::speculation_id(7, reason),
        })
    }

    /// No agent: nothing leaves. An interpreter-only JVMTI event: both bits,
    /// with or without a `JvmThread`, unless a class of the process was
    /// redefined, when only an OSR body (the innermost-frame bit) leaves.
    #[test]
    fn the_every_frame_bit_is_the_vm_wide_answer() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            let mut thread = JvmThread::new(ThreadId(0), "loop");
            thread.frames.push(frame_of("spin"));
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
            assert_eq!(compiled_frame_exit_verdict(&shared, None, None), 0);
            assert!(!every_compiled_frame_may_leave(&shared));

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
            if crate::classloading::any_class_redefined() {
                // Another test of this binary redefined a class.
                assert!(!every_compiled_frame_may_leave(&shared));
                assert_eq!(
                    compiled_frame_exit_verdict(&shared, Some(&thread), None),
                    cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                );
                assert_eq!(compiled_frame_exit_verdict(&shared, None, None), 0);
            } else {
                assert!(every_compiled_frame_may_leave(&shared));
                assert_eq!(
                    compiled_frame_exit_verdict(&shared, Some(&thread), None),
                    both
                );
                assert_eq!(
                    compiled_frame_exit_verdict(&shared, None, None),
                    both,
                    "the VM-wide answer needs no thread"
                );
            }

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// An exit is "left for the interpreter" exactly when it is an `OsrExit`
    /// and the mode holds; a stashed frame is judged by the cause beside it.
    #[test]
    fn only_an_osr_exit_under_the_mode_is_left_for_the_interpreter() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            let id = ClassId::new(0);
            let frame = stashed("T.spin:()V");
            assert!(!exit_left_for_the_interpreter(
                &shared,
                DeoptReason::OsrExit,
                id,
                "spin",
                "()V"
            ));
            assert!(!stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::OsrExit),
                0
            ));

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            assert!(exit_left_for_the_interpreter(
                &shared,
                DeoptReason::OsrExit,
                id,
                "spin",
                "()V"
            ));
            assert!(
                !exit_left_for_the_interpreter(
                    &shared,
                    DeoptReason::BoundsCheck,
                    id,
                    "spin",
                    "()V"
                ),
                "a guard is a failed speculation whatever the mode"
            );
            assert!(stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::OsrExit),
                0
            ));
            assert!(!stashed_exit_left_for_the_interpreter(
                &shared, &frame, None, 0
            ));
            assert!(!stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::UncommonTrap),
                0
            ));
            assert!(
                !stashed_exit_left_for_the_interpreter(
                    &shared,
                    &stashed(""),
                    cause(DeoptReason::OsrExit),
                    0
                ),
                "a key-less frame names no method"
            );
            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// The charging sinks: while the mode holds, an `OsrExit` is neither
    /// recorded by `DeoptimizationController::deoptimize_in` nor by
    /// `helpers::despeculate_trapped_method`, and a guard still is; with the
    /// mode off the same `OsrExit` is charged as before.
    #[test]
    fn the_charging_sinks_decline_an_exit_left_for_the_interpreter() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            let id = ClassId::new(0);
            let key = "I15L3Charge.spin:()V";
            let at_site = |reason: DeoptReason| {
                shared
                    .jit
                    .deopt_log
                    .lock()
                    .deopt_count_at_site(key, reason, 7)
            };

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            let action = crate::jit::helpers::DeoptimizationController::deoptimize_in(
                &shared,
                id,
                "I15L3Charge",
                "spin",
                "()V",
                DeoptReason::OsrExit,
                7,
            );
            assert_eq!(action, cratonvm_jit::deopt::DeoptAction::Reinterpret);
            crate::jit::helpers::despeculate_trapped_method(
                &shared,
                "I15L3Charge",
                "spin",
                "()V",
                id,
                DeoptReason::OsrExit,
                7,
            );
            assert_eq!(
                at_site(DeoptReason::OsrExit),
                0,
                "the agent's exit is not charged"
            );
            crate::jit::helpers::DeoptimizationController::deoptimize_in(
                &shared,
                id,
                "I15L3Charge",
                "spin",
                "()V",
                DeoptReason::BoundsCheck,
                7,
            );
            assert_eq!(
                at_site(DeoptReason::BoundsCheck),
                1,
                "a guard is still charged"
            );

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            crate::jit::helpers::DeoptimizationController::deoptimize_in(
                &shared,
                id,
                "I15L3Charge",
                "spin",
                "()V",
                DeoptReason::OsrExit,
                7,
            );
            assert_eq!(
                at_site(DeoptReason::OsrExit),
                1,
                "with the mode off an OsrExit is charged as before"
            );
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 17, lane L1
/// (`i15-L3-proposal-poll-slow-path-names-its-compiled-body`): the polling-body
/// verdict bit is about the body the poll named.
#[cfg(test)]
mod i17_l1_polling_body_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    const BODY_CLASS: u32 = 70;

    fn frame_of(method_name: &str) -> Frame {
        Frame::new(
            ClassId::new(0),
            "Other".to_string(),
            method_name.to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    /// `shared` with class `T` loaded as id [`BODY_CLASS`].
    fn vm_with_body_class() -> Arc<SharedVm> {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        shared.classes.class_manager.write().register_class_name(
            cratonvm_types::ClassLoaderId::Application,
            "T",
            ClassId::new(BODY_CLASS),
        );
        shared
    }

    #[test]
    fn an_artifact_names_its_body_by_owner_and_method_key() {
        let body = PollingBody::from_artifact(BODY_CLASS, "p/T.spin:(I)V")
            .expect("a published method key");
        assert_eq!(body.class_id, ClassId::new(BODY_CLASS));
        assert_eq!(body.class_name, "p/T");
        assert_eq!(body.method_name, "spin");
        assert_eq!(body.descriptor, "(I)V");
        assert!(
            PollingBody::from_artifact(cratonvm_types::jit_activation::NO_OWNER_CLASS, "T.m:()V")
                .is_none(),
            "never published"
        );
        assert!(PollingBody::from_artifact(1, "lambda-adapter->0x1000").is_none());
        assert!(PollingBody::from_artifact(1, "T.m").is_none());
        assert!(PollingBody::from_artifact(1, ".m:()V").is_none());
    }

    /// With no agent a named body is told nothing, and the negative fast path
    /// the slow path asks first agrees.
    #[test]
    fn no_agent_tells_a_named_body_nothing() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            let mut thread = JvmThread::new(ThreadId(0), "loop");
            thread.frames.push(frame_of("caller"));
            let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");
            assert!(!compiled_frames_may_be_asked_to_leave(&shared));
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), body), 0);
            assert_eq!(compiled_frame_exit_verdict(&shared, None, body), 0);
        })
        .join()
        .expect("test thread");
    }

    /// An interpreter-only JVMTI event concerns every method: a named body is
    /// told to leave whatever class of the process was redefined, because the
    /// redefinition clause is its own class's (the wave-15 answer withheld
    /// every method-entry exit once any class had been).
    #[test]
    fn a_named_body_leaves_under_the_vm_wide_mode_after_an_unrelated_redefinition() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            let vm = shared.vm_identity;
            let mut thread = JvmThread::new(ThreadId(0), "loop");
            thread.frames.push(frame_of("caller"));
            let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            assert!(compiled_frames_may_be_asked_to_leave(&shared));
            let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
            // Class `T` of this VM was never redefined, so this holds whether
            // or not another test of the binary redefined some class.
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), body),
                both
            );
            // The unnamed body keeps the process-wide clause.
            let unnamed = compiled_frame_exit_verdict(&shared, None, None);
            if crate::classloading::any_class_redefined() {
                assert_eq!(unnamed, 0);
            } else {
                assert_eq!(unnamed, both);
            }

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), body), 0);
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// Interpreter round i2 wave 53, lane L2 (the wave-25 design of the i22-L2
    /// caller-held proposal): `SAFEPOINT_VERDICT_NAMED_BODY` is set exactly for
    /// a named body that must leave and that no compiled caller bound for a
    /// caller-held CALL -- never for a bound one, one of unknown binding, the
    /// unnamed body, or a body that need not leave.
    #[test]
    fn the_named_body_bit_spares_a_body_bound_by_a_caller_held_site() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            let vm = shared.vm_identity;
            let mut thread = JvmThread::new(ThreadId(0), "loop");
            thread.frames.push(frame_of("caller"));
            let unbound = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
                .map(|b| b.with_caller_held_binding(false));
            let bound = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
                .map(|b| b.with_caller_held_binding(true));
            let unknown = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");
            let named = cratonvm_jit::SAFEPOINT_VERDICT_NAMED_BODY;
            let has_named = |v: i64| v & named != 0;

            // Nothing must leave: no bit for anyone.
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), unbound),
                0
            );

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), unbound),
                both | named,
                "an unbound named body leaves on the named bit too"
            );
            let spared = compiled_frame_exit_verdict(&shared, Some(&thread), bound);
            assert_eq!(spared, both, "a bound body keeps every other bit");
            assert!(!has_named(spared));
            assert!(
                !has_named(compiled_frame_exit_verdict(&shared, Some(&thread), unknown)),
                "an unknown binding is treated as bound"
            );
            assert!(
                !has_named(compiled_frame_exit_verdict(&shared, Some(&thread), None)),
                "the unnamed body never gets the named bit"
            );

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), unbound),
                0
            );
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// A JDWP breakpoint concerns its own method only: that method's body is
    /// told to leave, another method's body and an unnamed body are not, and
    /// neither is a body whose class name resolves to another class (a
    /// by-name sink would charge its exit).
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn a_breakpoint_pulls_back_the_body_of_its_own_method_only() {
        use crate::debug::{events, jdwp_method_id, publish_debugger_gates, DebugState};
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
                return; // a JVMTI listener already keeps everything interpreted
            }
            let mut thread = JvmThread::new(ThreadId(0), "loop");
            thread.frames.push(frame_of("caller"));
            let mut ds = DebugState::new();
            ds.events.set_event_request(
                events::EventKind::Breakpoint,
                events::SuspendPolicy::None,
                vec![events::EventModifier::LocationOnly {
                    class_id: u64::from(BODY_CLASS),
                    method_id: jdwp_method_id("hot", "()V"),
                    offset: 0,
                }],
            );
            // The same method of another class `T`, which the name does not
            // resolve to in this VM.
            ds.events.set_event_request(
                events::EventKind::Breakpoint,
                events::SuspendPolicy::None,
                vec![events::EventModifier::LocationOnly {
                    class_id: u64::from(BODY_CLASS + 1),
                    method_id: jdwp_method_id("hot", "()V"),
                    offset: 0,
                }],
            );
            publish_debugger_gates(&shared, &ds);
            assert!(compiled_frames_may_be_asked_to_leave(&shared));

            let hot = PollingBody::from_artifact(BODY_CLASS, "T.hot:()V");
            let cold = PollingBody::from_artifact(BODY_CLASS, "T.cold:()V");
            let alias = PollingBody::from_artifact(BODY_CLASS + 1, "T.hot:()V");
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), hot),
                cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
                "the body of the method holding the breakpoint leaves"
            );
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), cold), 0);
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), alias),
                0
            );
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), None),
                0,
                "an unnamed body is told only what concerns every method"
            );

            ds.events.clear_all();
            publish_debugger_gates(&shared, &ds);
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), hot), 0);
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 18, lane L1: a JVMTI `Exception` listener and
/// compiled code.
#[cfg(test)]
mod i18_l1_exception_listener_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    /// Page `interpreter-L1-jvmti-exception-events-lost-for-throws-caught-in-compiled-code-FIXED-20260925`:
    /// only the unwinder posts `Exception`, so while an agent listens for it
    /// the JIT's doors stand down and every compiled frame of the VM is told
    /// to leave at its next poll, exactly as for `MethodEntry`. Nothing is
    /// asked once it stops listening.
    #[test]
    fn an_exception_listener_sends_compiled_frames_to_the_interpreter() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            let mut thread = JvmThread::new(ThreadId(0), "thrower");
            thread.frames.push(Frame::new(
                ClassId::new(0),
                "T".to_string(),
                "spin".to_string(),
                "()V".to_string(),
                None,
                vec![0xb1],
                vec![],
                8,
                4,
                &[],
            ));
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
            assert!(!crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter(&shared));

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
                .expect("enable Exception");
            // Wave 92 (lane L1): the VM's own answer, which
            // `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` turns off.
            if mgr.exception_events_need_the_interpreter() {
                assert!(
                    crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter(&shared),
                    "the doors stand down"
                );
                assert!(compiled_frames_may_be_asked_to_leave(&shared));
                let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                    | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
                let expected = if crate::classloading::any_class_redefined() {
                    // Another test of this binary redefined a class: only an
                    // OSR body (the innermost-frame bit) may leave.
                    cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                } else {
                    both
                };
                assert_eq!(
                    compiled_frame_exit_verdict(&shared, Some(&thread), None),
                    expected
                );
            }

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
                .expect("disable Exception");
            assert!(!crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter(&shared));
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 18, lane L3
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, method-entry items
/// (b) and (c)): the mode exit the slow path grants a named body is recorded
/// with the body's class and point range, so the sinks that hold only the
/// stash judge it by class id, and a body compiled after its class was
/// redefined leaves (and is resumed) where an obsolete one does not.
#[cfg(test)]
mod i18_l3_mode_exit_grant_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{DeoptCause, DeoptReason, ReconstructedFrame, ResumeSemantics};
    use std::sync::Arc;

    const BODY_CLASS: u32 = 71;
    /// A stand-in for an artifact's `deopt_points` range.
    const POINTS: (usize, usize) = (0x10_000, 0x10_400);

    fn vm_with_body_class() -> Arc<SharedVm> {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        shared.classes.class_manager.write().register_class_name(
            cratonvm_types::ClassLoaderId::Application,
            "T",
            ClassId::new(BODY_CLASS),
        );
        shared
    }

    fn caller_thread() -> JvmThread {
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(Frame::new(
            ClassId::new(0),
            "Other".to_string(),
            "caller".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        thread
    }

    fn stashed(method_key: &str) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: method_key.to_string(),
            bci: 2,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        }
    }

    fn cause(reason: DeoptReason) -> Option<DeoptCause> {
        Some(DeoptCause {
            reason,
            speculation_id: cratonvm_jit::deopt::speculation_id(2, reason),
        })
    }

    /// A body with its artifact facts is recorded when it is told to leave,
    /// and only then; the record answers for a point inside its range, of
    /// this VM, and nothing else.
    #[test]
    fn a_granted_exit_is_recorded_with_the_bodys_class_and_points() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            let vm = shared.vm_identity;
            let thread = caller_thread();
            let epoch = cratonvm_jit::jit_install_epoch();
            let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
                .map(|b| b.with_artifact_facts(POINTS, epoch));
            let inside = POINTS.0 + 0x40;

            // No agent: not told to leave, nothing recorded.
            assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), body), 0);
            assert_eq!(mode_exit_grant_for(&shared, inside), None);

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            let verdict = compiled_frame_exit_verdict(&shared, Some(&thread), body);
            assert_ne!(verdict & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY, 0);
            let grant = mode_exit_grant_for(&shared, inside).expect("the exit is recorded");
            assert_eq!(grant.class_id, ClassId::new(BODY_CLASS));
            assert_eq!(mode_exit_grant_for(&shared, 0), None, "0 is never a point");
            assert_eq!(mode_exit_grant_for(&shared, POINTS.1), None, "past the end");
            assert_eq!(mode_exit_grant_for(&shared, POINTS.0 - 8), None, "before");
            let other = Arc::new(SharedVm::new(VmConfig::default()));
            assert_eq!(
                mode_exit_grant_for(&other, inside),
                None,
                "another VM's point is not this record's"
            );

            // The stash sink judges the exit by the record.
            let frame = stashed("T.spin:()V");
            assert!(stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::OsrExit),
                inside
            ));
            assert!(!stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::BoundsCheck),
                inside
            ));
            // Compiled after every flush of this VM's cache: current.
            assert!(granted_exit_of_a_current_body(
                &shared,
                cause(DeoptReason::OsrExit),
                inside
            ));
            assert!(!granted_exit_of_a_current_body(
                &shared,
                cause(DeoptReason::UncommonTrap),
                inside
            ));
            assert!(!granted_exit_of_a_current_body(&shared, None, inside));
            // A flush after the grant (a redefinition) makes the same record
            // name an obsolete body: its currency is re-read at each use.
            shared.jit.jit_cache.clear_all();
            assert!(!granted_exit_of_a_current_body(
                &shared,
                cause(DeoptReason::OsrExit),
                inside
            ));

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// Item (b): a body compiled after its class's redefinition leaves under
    /// the VM-wide mode; a body compiled before it (an obsolete body, which no
    /// sink can resume) does not, and neither does one whose age is unknown.
    #[test]
    fn a_body_compiled_after_its_class_was_redefined_leaves_and_an_obsolete_one_does_not() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            let vm = shared.vm_identity;
            let thread = caller_thread();
            // Class `T` redefined once, and the VM's cache flushed for it, as
            // `redefine_class_with` does.
            shared
                .classes
                .class_manager
                .read()
                .class_redefine_generation_handle(ClassId::new(BODY_CLASS))
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let before = cratonvm_jit::jit_install_epoch();
            shared.jit.jit_cache.clear_all();
            let after = cratonvm_jit::jit_install_epoch();
            let current = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
                .map(|b| b.with_artifact_facts(POINTS, after));
            let obsolete = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
                .map(|b| b.with_artifact_facts((0x20_000, 0x20_400), before));
            let unknown = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            let polling = |body: Option<PollingBody<'_>>| {
                compiled_frame_exit_verdict(&shared, Some(&thread), body)
                    & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY
            };
            // `class_was_redefined` needs the process latch, which only a real
            // redefinition raises; without it the generation bump is unseen
            // and every named body leaves, as before this wave.
            let redefined_seen = crate::runtime::redefine_state::class_was_redefined(
                &shared,
                ClassId::new(BODY_CLASS),
            );
            if redefined_seen {
                assert_eq!(polling(obsolete), 0, "an obsolete body stays compiled");
                assert_eq!(polling(unknown), 0, "an unknown age is not current");
            } else {
                assert_ne!(polling(obsolete), 0);
                assert_ne!(polling(unknown), 0);
            }
            assert_ne!(polling(current), 0, "a current body leaves");
            // Its exit is the one the sinks may resume against the class as it
            // is now.
            assert!(granted_exit_of_a_current_body(
                &shared,
                cause(DeoptReason::OsrExit),
                POINTS.0
            ));

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// Item (c): a JDWP breakpoint in the method of a class whose name
    /// resolves to ANOTHER class of the same name now pulls that class's body
    /// back when the poll knows its artifact facts, and the stash sink judges
    /// the exit by the recorded class, not by the name (wave 17 withheld it:
    /// `a_breakpoint_pulls_back_the_body_of_its_own_method_only`, which keeps
    /// covering a body without facts).
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn a_same_named_class_body_leaves_and_its_exit_is_judged_by_class_id() {
        use crate::debug::{events, jdwp_method_id, publish_debugger_gates, DebugState};
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
                return; // a JVMTI listener already keeps everything interpreted
            }
            let thread = caller_thread();
            let alias_class = BODY_CLASS + 1;
            let mut ds = DebugState::new();
            ds.events.set_event_request(
                events::EventKind::Breakpoint,
                events::SuspendPolicy::None,
                vec![events::EventModifier::LocationOnly {
                    class_id: u64::from(alias_class),
                    method_id: jdwp_method_id("hot", "()V"),
                    offset: 0,
                }],
            );
            publish_debugger_gates(&shared, &ds);

            let epoch = cratonvm_jit::jit_install_epoch();
            let alias = PollingBody::from_artifact(alias_class, "T.hot:()V")
                .map(|b| b.with_artifact_facts(POINTS, epoch));
            let own = PollingBody::from_artifact(BODY_CLASS, "T.hot:()V")
                .map(|b| b.with_artifact_facts((0x30_000, 0x30_400), epoch));
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), own),
                0,
                "the name's own class holds no breakpoint"
            );
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), alias),
                cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
                "the class holding the breakpoint leaves, whatever its name resolves to"
            );
            let frame = stashed("T.hot:()V");
            assert!(
                stashed_exit_left_for_the_interpreter(
                    &shared,
                    &frame,
                    cause(DeoptReason::OsrExit),
                    POINTS.0 + 0x40
                ),
                "judged by the recorded class: not charged"
            );
            assert!(
                !stashed_exit_left_for_the_interpreter(
                    &shared,
                    &frame,
                    cause(DeoptReason::OsrExit),
                    0
                ),
                "without the point, by name: the name's own class holds no breakpoint"
            );

            ds.events.clear_all();
            publish_debugger_gates(&shared, &ds);
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), alias),
                0
            );
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 19, lane L2
/// (`interpreter-L2-ir-bodies-get-no-mode-exit-grant-FIXED-20260925`): an optimizing body's poll
/// exits are boxes outside its `deopt_points`, and the mode-exit grant now
/// keeps their addresses, so the sinks that hold only the stash match an
/// optimizing body's exit exactly as a single-pass body's: by class id, and
/// as the exit of a body compiled after its class was redefined.
#[cfg(test)]
mod i19_l2_ir_grant_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use crate::threading::jvm_thread::ThreadId;
    use cratonvm_jit::deopt::{
        speculation_id, DeoptAction, DeoptCause, DeoptReason, DeoptimizationPoint, FrameState,
        FrameValue, ReconstructedFrame, ResumeSemantics,
    };
    use std::sync::Arc;

    const BODY_CLASS: u32 = 73;

    fn vm_with_body_class() -> Arc<SharedVm> {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        shared.classes.class_manager.write().register_class_name(
            cratonvm_types::ClassLoaderId::Application,
            "T",
            ClassId::new(BODY_CLASS),
        );
        shared
    }

    fn caller_thread() -> JvmThread {
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(Frame::new(
            ClassId::new(0),
            "Other".to_string(),
            "caller".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        thread
    }

    /// A boxed point at bci 2, as the optimizing lowerer publishes a
    /// back-edge poll's mode exit (`OsrExit`) or a guard.
    fn boxed(reason: DeoptReason) -> Box<DeoptimizationPoint> {
        Box::new(DeoptimizationPoint {
            native_offset: 0,
            bci: 2,
            reason,
            action: DeoptAction::Reinterpret,
            speculation_id: speculation_id(2, reason),
            frame_state: FrameState {
                method_key: String::new(),
                bci: 2,
                locals: vec![FrameValue::StackSlot(-8)],
                stack: Vec::new(),
                monitors: Vec::new(),
                caller: None,
            },
            semantics: ResumeSemantics::for_reason(reason),
        })
    }

    fn address(point: &DeoptimizationPoint) -> usize {
        // Cast: a point address, compared only.
        point as *const DeoptimizationPoint as usize
    }

    fn stashed(method_key: &str) -> ReconstructedFrame {
        ReconstructedFrame {
            method_key: method_key.to_string(),
            bci: 2,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            semantics: ResumeSemantics::REEXECUTE,
            caller_frames: Vec::new(),
        }
    }

    fn cause(reason: DeoptReason) -> Option<DeoptCause> {
        Some(DeoptCause {
            reason,
            speculation_id: speculation_id(2, reason),
        })
    }

    /// A body whose only boxes are guards names no point a mode exit can
    /// stash; one with a boxed `OsrExit` does, and its grant matches exactly
    /// its `OsrExit` boxes: not a guard box, not an address inside a box, and
    /// not after a later grant replaced it.
    #[test]
    fn an_optimizing_bodys_poll_exit_is_matched_by_its_box() {
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            let vm = shared.vm_identity;
            let thread = caller_thread();
            let epoch = cratonvm_jit::jit_install_epoch();
            let boxes = vec![
                boxed(DeoptReason::OsrExit),
                boxed(DeoptReason::NullCheck),
                boxed(DeoptReason::OsrExit),
            ];
            let (exit, guard, other_exit) =
                (address(&boxes[0]), address(&boxes[1]), address(&boxes[2]));
            // An optimizing artifact's `deopt_points` are by-value copies whose
            // addresses no stub bakes; empty here.
            let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V").map(|b| {
                b.with_artifact_facts((0, 0), epoch)
                    .with_boxed_exit_points(&boxes)
            });
            assert!(body.is_some_and(|b| b.exit_points_known()));
            let guards = vec![boxed(DeoptReason::NullCheck)];
            let guarded = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V").map(|b| {
                b.with_artifact_facts((0, 0), epoch)
                    .with_boxed_exit_points(&guards)
            });
            assert!(!guarded.is_some_and(|b| b.exit_points_known()));

            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            let verdict = compiled_frame_exit_verdict(&shared, Some(&thread), body);
            assert_ne!(verdict & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY, 0);
            let grant = mode_exit_grant_for(&shared, exit).expect("the exit is recorded");
            assert_eq!(grant.class_id, ClassId::new(BODY_CLASS));
            assert!(mode_exit_grant_for(&shared, other_exit).is_some());
            assert_eq!(
                mode_exit_grant_for(&shared, guard),
                None,
                "a guard is no mode exit"
            );
            assert_eq!(
                mode_exit_grant_for(&shared, exit + 1),
                None,
                "not a point address"
            );

            // The sinks that hold only the stash.
            let frame = stashed("T.spin:()V");
            assert!(stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::OsrExit),
                exit
            ));
            assert!(granted_exit_of_a_current_body(
                &shared,
                cause(DeoptReason::OsrExit),
                other_exit
            ));
            assert!(!granted_exit_of_a_current_body(
                &shared,
                cause(DeoptReason::OsrExit),
                guard
            ));

            // A later grant (a single-pass body) replaces the record.
            let single_pass = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
                .map(|b| b.with_artifact_facts((0x10_000, 0x10_400), epoch));
            assert_ne!(
                compiled_frame_exit_verdict(&shared, Some(&thread), single_pass)
                    & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
                0
            );
            assert!(mode_exit_grant_for(&shared, 0x10_040).is_some());
            assert_eq!(mode_exit_grant_for(&shared, exit), None, "replaced");

            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }

    /// For an optimizing body: a JDWP breakpoint in the method of a class
    /// whose name resolves to ANOTHER class pulls that class's body back once
    /// the poll knows its boxed exits (without them, the wave-17 rule withholds
    /// the exit), and the stash sink judges the exit by the recorded class.
    #[cfg(feature = "experimental-debug")]
    #[test]
    fn a_same_named_class_optimizing_body_leaves_and_is_judged_by_class_id() {
        use crate::debug::{events, jdwp_method_id, publish_debugger_gates, DebugState};
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = vm_with_body_class();
            if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
                return; // a JVMTI listener already keeps everything interpreted
            }
            let thread = caller_thread();
            let alias_class = BODY_CLASS + 1;
            let mut ds = DebugState::new();
            ds.events.set_event_request(
                events::EventKind::Breakpoint,
                events::SuspendPolicy::None,
                vec![events::EventModifier::LocationOnly {
                    class_id: u64::from(alias_class),
                    method_id: jdwp_method_id("hot", "()V"),
                    offset: 0,
                }],
            );
            publish_debugger_gates(&shared, &ds);

            let epoch = cratonvm_jit::jit_install_epoch();
            let boxes = vec![boxed(DeoptReason::OsrExit)];
            let exit = address(&boxes[0]);
            let without_facts = PollingBody::from_artifact(alias_class, "T.hot:()V");
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), without_facts),
                0,
                "the wave-17 rule: the name resolves to another class"
            );
            let alias = PollingBody::from_artifact(alias_class, "T.hot:()V").map(|b| {
                b.with_artifact_facts((0, 0), epoch)
                    .with_boxed_exit_points(&boxes)
            });
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), alias),
                cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
                "the class holding the breakpoint leaves, whatever its name resolves to"
            );
            let frame = stashed("T.hot:()V");
            assert!(
                stashed_exit_left_for_the_interpreter(
                    &shared,
                    &frame,
                    cause(DeoptReason::OsrExit),
                    exit
                ),
                "judged by the recorded class: not charged"
            );

            ds.events.clear_all();
            publish_debugger_gates(&shared, &ds);
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 23, lane L6
/// (`docs/internal/fixed-bugs/interpreter-L6-a-running-splice-of-a-redefined-callee-runs-its-old-bytecode-FIXED-20260926.md`):
/// a body a class redefinition withdrew is told to leave with no agent
/// listening, on both verdict bits, when its exit points are known, and its
/// exit is then not charged as a failed speculation.
#[cfg(test)]
mod i23_l6_withdrawn_body_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::threading::jvm_thread::ThreadId;
    use std::sync::Arc;

    const BODY_CLASS: u32 = 2306;
    const LABEL: &str = "i23l6/Withdrawn.spin:()V";

    #[test]
    fn a_withdrawn_body_leaves_with_no_agent_listening() {
        std::thread::spawn(|| {
            let _lock = crate::runtime::jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            shared.classes.class_manager.write().register_class_name(
                cratonvm_types::ClassLoaderId::Application,
                "i23l6/Withdrawn",
                ClassId::new(BODY_CLASS),
            );
            let thread = JvmThread::new(ThreadId(0), "loop");
            let known = |withdrawn: bool| {
                PollingBody::from_artifact(BODY_CLASS, LABEL).map(|body| {
                    body.with_artifact_facts((0x1000, 0x1040), 0)
                        .with_withdrawn_by_redefinition(withdrawn)
                })
            };
            assert!(known(true).is_some());
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), known(false)),
                0,
                "no agent, not withdrawn: nothing"
            );
            // No exit point known: no sink could match its stash.
            let unknown = PollingBody::from_artifact(BODY_CLASS, LABEL)
                .map(|body| body.with_withdrawn_by_redefinition(true));
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), unknown),
                0
            );
            let osr = cratonvm_jit::deopt::DeoptReason::OsrExit;
            assert!(!exit_left_for_the_interpreter(
                &shared,
                osr,
                ClassId::new(BODY_CLASS),
                "spin",
                "()V"
            ));
            let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), known(true)),
                both,
                "withdrawn: an OSR body tests the one bit, a method-entry body the other"
            );
            // The grant the verdict recorded makes the exit a mode exit for
            // the sinks: resumed, not charged.
            assert!(exit_left_for_the_interpreter(
                &shared,
                osr,
                ClassId::new(BODY_CLASS),
                "spin",
                "()V"
            ));
            assert!(!exit_left_for_the_interpreter(
                &shared,
                cratonvm_jit::deopt::DeoptReason::UncommonTrap,
                ClassId::new(BODY_CLASS),
                "spin",
                "()V"
            ));
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 37, lane L1: a request that needs every method
/// interpreted withdraws every compiled body once, on the edge where its first
/// source arms, and the tier-up gate stays closed until the last source goes
/// ([`note_every_method_needs_the_interpreter`]).
#[cfg(all(test, feature = "experimental-debug"))]
mod i37_l1_every_body_withdrawal_tests {
    use super::*;
    use crate::config::VmConfig;
    use crate::debug::{WITHDRAWAL_BY_JDWP, WITHDRAWAL_BY_JVMTI};
    use crate::jit::tiered::{CompilationTier, MethodKey};
    use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
    use std::sync::Arc;

    /// A one-`RET` body, publishable under any key.
    fn body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        cratonvm_jit::CompiledMethod::new(buf)
    }

    #[test]
    fn the_first_source_withdraws_every_body_and_the_last_reopens_the_gate() {
        if !EVERY_BODY_WITHDRAWAL_ENABLED {
            return;
        }
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let id = ClassId::new(0x3701);
        let key = MethodKey::with_class_id(id, "i37/l1/Hot", "run", "()V");
        let tiered = &shared.jit.tiered_manager;
        tiered.on_method_invocation(&key);
        tiered.compilation_complete(&key, CompilationTier::C1, 1);
        shared.jit.jit_cache.put(
            Arc::from("i37/l1/Hot"),
            Arc::from("run"),
            Arc::from("()V"),
            id,
            body(),
        );
        let published = shared
            .jit
            .jit_cache
            .get("i37/l1/Hot", "run", "()V", id)
            .expect("published");
        let gates = &shared.debug.debugger_gates;
        assert!(!gates.every_body_withdrawn(), "nothing without a debugger");
        let before = shared.jit.code_state_epoch();

        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
        assert!(gates.every_body_withdrawn());
        assert!(shared
            .jit
            .jit_cache
            .get("i37/l1/Hot", "run", "()V", id)
            .is_none());
        assert!(
            published.is_withdrawn_by_redefinition(),
            "a frame running it leaves at its next exit-capable point"
        );
        assert!(shared
            .jit
            .jit_cache
            .body_withdrawn_by_redefinition(&published));
        let withdrawn_at = shared.jit.code_state_epoch();
        assert!(withdrawn_at > before, "queued compile requests expire");
        if shared.jit.jit_cache.reports_withdrawals() {
            assert_eq!(tiered.current_tier(&key), CompilationTier::Interpreter);
        }

        // A second source finds the code withdrawn already.
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JVMTI, true);
        assert_eq!(shared.jit.code_state_epoch(), withdrawn_at);
        // The gate stays closed while either source is armed.
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
        assert!(gates.every_body_withdrawn());
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JVMTI, false);
        assert!(!gates.every_body_withdrawn(), "no source is armed");
        assert_eq!(
            gates.tier_up_held(),
            crate::debug::WITHDRAWAL_LINGER_MS != 0,
            "the tier-up strides stay closed for the linger (wave 39)"
        );
        assert_eq!(
            shared.jit.code_state_epoch(),
            withdrawn_at,
            "a disarm flushes nothing"
        );

        // Wave 39: the next request finds nothing compiled since, and takes
        // no second flush (a debugger's next step); queued compiles still
        // expire.
        let held_before = gates.withdrawals_held();
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
        assert!(gates.every_body_withdrawn());
        assert_eq!(gates.withdrawals_held(), held_before + 1, "no second flush");
        let fenced_at = shared.jit.code_state_epoch();
        assert!(fenced_at > withdrawn_at, "queued compile requests expire");
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);

        // Something compiled meanwhile (a door that does not ask the strides):
        // the next request withdraws again.
        shared.jit.jit_cache.put(
            Arc::from("i37/l1/Hot"),
            Arc::from("run"),
            Arc::from("()V"),
            id,
            body(),
        );
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
        assert_eq!(gates.withdrawals_held(), held_before + 1, "withdrawn again");
        assert!(shared.jit.code_state_epoch() > fenced_at);
        assert!(shared
            .jit
            .jit_cache
            .get("i37/l1/Hot", "run", "()V", id)
            .is_none());
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
        assert!(!gates.every_body_withdrawn());
    }

    /// Wave 39: the linger ends; a disarm of a source that was not armed
    /// starts none.
    #[test]
    fn a_disarm_without_an_armed_source_starts_no_linger() {
        if !EVERY_BODY_WITHDRAWAL_ENABLED {
            return;
        }
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let gates = &shared.debug.debugger_gates;
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
        assert!(!gates.tier_up_held(), "nothing was armed");
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
        assert!(gates.tier_up_held());
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
        assert_eq!(
            gates.tier_up_held(),
            crate::debug::WITHDRAWAL_LINGER_MS != 0
        );
        assert!(!gates.every_body_withdrawn());
        // A re-arm ends the linger; the bit holds the strides instead.
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
        assert!(!gates.end_withdrawal_linger(), "the arm ended it");
        assert!(gates.tier_up_held());
        note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
    }

    /// The JVMTI source: a `MethodEntry` listener of the VM's manager raises
    /// it (through `jvmti::publish_union_listener_flags`) and disabling the
    /// event lowers it; an `Exception` listener does not, since the compiled
    /// catch doors post that event themselves.
    #[test]
    fn a_method_entry_listener_holds_the_code_withdrawn_while_it_listens() {
        if !EVERY_BODY_WITHDRAWAL_ENABLED {
            return;
        }
        std::thread::spawn(|| {
            let _lock = jvmti::jvmti_registry_test_lock();
            let shared = Arc::new(SharedVm::new(VmConfig::default()));
            let vm = shared.vm_identity;
            jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
            jvmti::install_real_agent_env_bridge(&shared);
            let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
            let gates = &shared.debug.debugger_gates;
            assert!(!gates.every_body_withdrawn());

            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
                .expect("enable Exception");
            assert!(
                !gates.every_body_withdrawn(),
                "Exception keeps the doors' answer only"
            );
            mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
                .expect("enable MethodEntry");
            assert!(gates.every_body_withdrawn());
            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
                .expect("disable MethodEntry");
            assert!(!gates.every_body_withdrawn());
            mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
                .expect("disable Exception");
            jvmti::forget_vm_jvmti_state(vm);
        })
        .join()
        .expect("test thread");
    }
}

/// Interpreter round i1 wave 38, lane L1: a breakpoint gained in a method of
/// one class withdraws that class's compiled bodies and leaves every other
/// class's compiled ([`note_breakpoint_classes_gained`]); a class the caller
/// cannot name takes the whole-cache withdrawal.
#[cfg(all(test, feature = "experimental-debug"))]
mod i38_l1_breakpoint_withdrawal_tests {
    use super::*;
    use crate::config::VmConfig;
    use std::sync::Arc;

    /// A one-`RET` body, publishable under any key.
    fn body() -> cratonvm_jit::CompiledMethod {
        let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
        buf.emit(&[0xC3]); // RET
        cratonvm_jit::CompiledMethod::new(buf)
    }

    fn publish(shared: &SharedVm, class: &str, method: &str, id: ClassId) {
        shared.jit.jit_cache.put(
            Arc::from(class),
            Arc::from(method),
            Arc::from("()V"),
            id,
            body(),
        );
    }

    #[test]
    fn a_breakpoint_withdraws_its_class_and_spares_the_others() {
        if !BREAKPOINT_SCOPED_WITHDRAWAL_ENABLED {
            return;
        }
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let bp = ClassId::new(0x3801);
        let other = ClassId::new(0x3802);
        publish(&shared, "i38/l1/Bp", "callee", bp);
        publish(&shared, "i38/l1/Bp", "loop", bp);
        publish(&shared, "i38/l1/Other", "run", other);
        let cache = &shared.jit.jit_cache;
        let callee = cache
            .get("i38/l1/Bp", "callee", "()V", bp)
            .expect("published");
        let before = shared.jit.code_state_epoch();

        note_breakpoint_classes_gained(&shared, &[(bp, Some("i38/l1/Bp".to_string()))]);
        assert!(cache.get("i38/l1/Bp", "callee", "()V", bp).is_none());
        assert!(cache.get("i38/l1/Bp", "loop", "()V", bp).is_none());
        assert!(
            callee.is_withdrawn_by_redefinition(),
            "a frame running it leaves at its next exit-capable point"
        );
        assert!(
            cache.get("i38/l1/Other", "run", "()V", other).is_some(),
            "another class's body stays compiled"
        );
        assert_eq!(
            shared.jit.code_state_epoch(),
            before,
            "a scoped withdrawal expires no queued compile"
        );
        assert!(
            cache.compiled_since_redefinition_of(bp, callee.install_epoch),
            "no redefinition barrier for a class that was not redefined"
        );
        assert!(!shared.debug.debugger_gates.every_body_withdrawn());

        // A JDK class, whose methods the JIT's intrinsics expand without a
        // record, takes the whole-cache path; a program's class does not.
        assert!(jit_may_expand_a_method_of_without_a_record(
            "java/lang/String"
        ));
        assert!(!jit_may_expand_a_method_of_without_a_record("i38/l1/Bp"));

        // A class the caller cannot name: every body goes.
        note_breakpoint_classes_gained(&shared, &[(ClassId::new(0x3803), None)]);
        assert!(cache.get("i38/l1/Other", "run", "()V", other).is_none());
        assert!(!shared.debug.debugger_gates.every_body_withdrawn());
    }
}

/// Interpreter round i4 wave 95, lane L7 (`CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`):
/// the dispatch loop's reader of the agent-request word takes a raised
/// request once, for the frame on top only, and clears it either way.
#[cfg(test)]
mod i95_l7_agent_request_word_tests {
    use super::*;
    use crate::threading::jvm_thread::{ThreadId, AGENT_REQUEST_FRAME_CONTROL};
    use std::sync::atomic::Ordering;

    fn frame() -> Frame {
        Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "m".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        )
    }

    #[test]
    fn a_raised_request_is_taken_once_for_the_top_frame() {
        let mut thread = JvmThread::new(ThreadId(9501), "agent-word");
        thread.frames.push(frame());
        thread.frames.push(frame());
        assert!(!take_method_entry_frame_control(&thread, 1), "nothing raised");
        thread
            .gc_block_state
            .agent_requests
            .fetch_or(AGENT_REQUEST_FRAME_CONTROL, Ordering::Relaxed);
        assert!(take_method_entry_frame_control(&thread, 1), "the top frame takes it");
        assert_eq!(
            thread.gc_block_state.agent_requests.load(Ordering::Relaxed),
            0,
            "taken: cleared"
        );
        assert!(!take_method_entry_frame_control(&thread, 1), "taken once");
    }

    #[test]
    fn a_request_seen_below_the_top_frame_is_dropped() {
        let mut thread = JvmThread::new(ThreadId(9502), "agent-word-below");
        thread.frames.push(frame());
        thread.frames.push(frame());
        thread
            .gc_block_state
            .agent_requests
            .fetch_or(AGENT_REQUEST_FRAME_CONTROL, Ordering::Relaxed);
        assert!(!take_method_entry_frame_control(&thread, 0));
        assert_eq!(thread.gc_block_state.agent_requests.load(Ordering::Relaxed), 0);
    }
}

#[cfg(all(test, feature = "experimental-debug"))]
mod i40_l1_shadow_exit_index_tests {
    //! Interpreter round i6 wave 120, lane L1 (i40): the exit index of a
    //! reviewed-`Intrinsic` shadow's frame-only report.
    use super::last_return_index;

    /// The walk steps whole instructions: an operand byte equal to a return
    /// opcode is not a return.
    #[test]
    fn last_return_index_walks_instructions() {
        // `Class.getName`'s shape: aload_0; getfield #0x00b0; astore_1;
        // aload_1; ifnonnull +10; aload_0; invokevirtual #2; goto +4;
        // aload_1; areturn.
        let get_name = [
            0x2a, 0xb4, 0x00, 0xb0, 0x4c, 0x2b, 0xc7, 0x00, 0x0a, 0x2a, 0xb6, 0x00, 0x02, 0xa7,
            0x00, 0x04, 0x2b, 0xb0,
        ];
        assert_eq!(last_return_index(&get_name), Some(17));
        // `return`, then (unreachable) `sipush 0x00ac`: the return at 0.
        assert_eq!(last_return_index(&[0xb1, 0x11, 0x00, 0xac]), Some(0));
        // Two returns: the last one.
        assert_eq!(last_return_index(&[0x03, 0xac, 0x04, 0xac]), Some(3));
        // No return (`athrow`).
        assert_eq!(last_return_index(&[0x01, 0xbf]), None);
    }
}

/// Interpreter round i6 wave 134, lane L1 (i133-L8 shape 1,
/// `CRATONVM_JVMTI_CONTENDED_ENTRY_FRAME_CONTROL`): the deferred entry posts
/// for the frame it was recorded for only, and the in-loop door enters the
/// same callback point as the uncontended entry.
#[cfg(test)]
mod i134_l1_contended_entry_tests {
    use super::*;

    #[test]
    fn a_deferred_entry_names_only_its_own_top_frame() {
        assert!(deferred_entry_names_top(3, 3));
        assert!(!deferred_entry_names_top(0, 0), "no record");
        assert!(!deferred_entry_names_top(3, 4), "a frame above it");
        assert!(!deferred_entry_names_top(3, 2), "its frame is gone");
    }

    /// Source pin: the in-loop door falls back to the plain post with either
    /// switch off, and otherwise posts at the callback point.
    #[test]
    fn the_in_loop_door_posts_at_the_callback_point() {
        let src = include_str!("jvmti_events.rs").replace("\r\n", "\n");
        let start = src
            .find("pub(crate) fn fire_deferred_method_entry_in_loop(")
            .expect("the door");
        let rest = &src[start..];
        let body = &rest[..rest.find("\n}\n").expect("its end")];
        let gate = body
            .find("config.jvmti_contended_entry_frame_control")
            .expect("the switch");
        let plain = body
            .find("fire_deferred_method_entry(shared.vm_identity, thread)")
            .expect("the fallback");
        let point = body
            .find("fire_method_entry_at_callback_point(shared, thread)")
            .expect("the point");
        assert!(gate < plain && plain < point);
        let sync = include_str!("../../vm/vm_exec.rs").replace("\r\n", "\n");
        assert!(sync.contains("fire_deferred_method_entry_in_loop(shared, thread)"));
    }
}
