#[allow(unused_imports)]
use super::*;

/// Resume a complete Java stack restored from a virtual-thread continuation.
///
/// Unlike `execute`, no new root frame is created: every frame already carries
/// its exact bytecode PC, locals, operand stack, exception table, and method
/// identity. Dispatch begins at the youngest frame and may return/unwind
/// through all restored callers down to index zero.
pub fn resume_continuation(shared: &SharedVm, thread: &mut JvmThread) -> MethodCallResult {
    if thread.frames.is_empty() {
        return Ok(None);
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[CONTINUATION_RESUME] frames={}", thread.frames.len());
        for (index, frame) in thread.frames.iter().enumerate() {
            eprintln!(
                "  [{index}] {}.{}{} pc={} stack={}",
                frame.class_name(),
                frame.method_name(),
                frame.method_descriptor(),
                frame.pc,
                frame.stack.len(),
            );
        }
    }
    // JEP 109: a frame frozen before a redefinition of its class runs the
    // replaced body; this carrier may have moved its own frames past that
    // redefinition already (interpreter round i1 wave 22, lane L3).
    obsolete_frames::convert_thawed_frames(shared, thread);
    let result = match remount_action(shared, thread) {
        RemountAction::Run => execute_frame_from_index(shared, thread, 0),
        // An early wake of an uninterrupted sleep: unmount again for the rest.
        // The carrier's yield arm (`resume_virtual_continuation`) handles
        // this exactly like the native's own yield.
        RemountAction::Yield { wake_after_nanos } => {
            return Err(MethodCallFailed::InternalError(
                VmError::ContinuationYield { wake_after_nanos },
            ));
        }
        RemountAction::Throw(exc) => {
            // Raised in the leaf frame at its invoke of the yielding native,
            // as if that native had thrown: `unwind_to_handler` finds the
            // covering handler (popping frames without one) or hands the
            // throwable back at frame 0, where the tail below pops it.
            match thread.frames.last().map(|leaf| leaf.last_instr_pc) {
                Some(invoke_pc) => {
                    let mut frame_idx = thread.frames.len() - 1;
                    match unwind_to_handler(shared, thread, &mut frame_idx, 0, exc, invoke_pc) {
                        Ok(()) => execute_frame_from_index(shared, thread, 0),
                        Err(e) => Err(e),
                    }
                }
                None => Err(MethodCallFailed::ExceptionThrown(exc)),
            }
        }
        RemountAction::Fail(e) => Err(e),
    };
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        return result;
    }
    // Pops down to (and including) frame 0, with a reference result pinned
    // across the pops' FramePop callbacks (wave 4 lane L1).
    let mut result = result;
    pop_root_frames_keeping_result(shared, thread, 0, &mut result);
    result
}

/// What a remounted virtual-thread continuation does before its leaf frame
/// runs on (interpreter round i1 wave 24, lane L7; see
/// [`crate::threading::jvm_thread::VtYield`]).
pub(in crate::runtime::interpreter) enum RemountAction {
    /// Resume after the yielding native's invoke, as if it had returned.
    Run,
    /// Unmount again (a sleep woken early by something other than an
    /// interrupt or its deadline).
    Yield { wake_after_nanos: u64 },
    /// Raise this throwable in the leaf frame at its invoke.
    Throw(ObjectRef),
    /// The throwable could not be built.
    Fail(MethodCallFailed),
}

/// HotSpot's answers for a thread woken from a sleep or a wait:
/// `JavaThread::sleep_nanos` checks the interrupt before the deadline on every
/// wake and throws `InterruptedException("sleep interrupted")` (clearing the
/// flag); an `Object.wait`-style keyed wait throws a message-less
/// `InterruptedException`; a park returns, whatever woke it. The record is
/// consumed here, so a later remount of the same continuation (after an
/// ordinary park) never sees a stale sleep.
pub(in crate::runtime::interpreter) fn remount_action(shared: &SharedVm, thread: &mut JvmThread) -> RemountAction {
    use crate::threading::jvm_thread::VtYield;
    let reason = std::mem::replace(&mut thread.vt_yield, VtYield::Park);
    let message = match reason {
        VtYield::Park => return RemountAction::Run,
        // Interpreter round i4 wave 87, lane L6: the leaf re-executes its
        // `monitorenter` (pc and operand stack were left at it).
        VtYield::MonitorEnter => return RemountAction::Run,
        // Wave 89: an unmounted `Object.wait` re-acquires its monitor first.
        VtYield::MonitorWait => match crate::vm::vm_exec::vthread_wait_remount(shared, thread) {
            crate::vm::vm_exec::VthreadWaitRemount::Run => return RemountAction::Run,
            crate::vm::vm_exec::VthreadWaitRemount::Yield => {
                return RemountAction::Yield {
                    wake_after_nanos: 0,
                }
            }
            // `ObjectMonitor::wait`'s interrupt: no message.
            crate::vm::vm_exec::VthreadWaitRemount::Interrupted => None,
        },
        // `Continuation.doYield()` returns `0` ("yielded") into `yield0`,
        // which stands past its `invokestatic` (`vm::jdk_continuation`).
        VtYield::DoYield => {
            return match thread.frames.last_mut() {
                Some(leaf) => match leaf.stack.push(Value::Int(0)) {
                    Ok(()) => RemountAction::Run,
                    Err(e) => {
                        RemountAction::Fail(MethodCallFailed::InternalError(VmError::Runtime(e)))
                    }
                },
                None => RemountAction::Run,
            };
        }
        VtYield::Sleep { deadline } => {
            if !thread
                .interrupted
                .load(std::sync::atomic::Ordering::Acquire)
            {
                let remaining = match deadline {
                    Some(d) => d.saturating_duration_since(std::time::Instant::now()),
                    None => std::time::Duration::MAX,
                };
                if remaining.is_zero() {
                    return RemountAction::Run;
                }
                // Not yet: a stray `LockSupport.unpark` resubmitted the sleeper
                // (`unpark_virtual` resubmits any `Parked` continuation), and
                // `Thread.sleep` does not end on an unpark. The permit that
                // unpark consumed is the thread's again (HotSpot keeps it set
                // across a sleep).
                shared
                    .threads
                    .virtual_thread_manager
                    .restore_unpark_permit(thread.thread_id.0);
                thread.vt_yield = reason;
                return RemountAction::Yield {
                    // Cast: the clamp bounds the u128 nanos to u64::MAX first; the narrowing never truncates.
                    wake_after_nanos: remaining.as_nanos().clamp(1, u64::MAX as u128) as u64,
                };
            }
            // HotSpot's `JavaThread::interrupt` unparks the thread's `Parker`
            // too, and a sleep does not consume that permit: the next
            // `LockSupport.park` after the `InterruptedException` returns at
            // once. The resubmit that woke this continuation took the permit
            // (`unpark_virtual`); give it back (wave 25, lane L7).
            shared
                .threads
                .virtual_thread_manager
                .restore_unpark_permit(thread.thread_id.0);
            // Wave 48 (lane L1): HotSpot's DEFAULT virtual threads throw from
            // `VirtualThread.sleepNanos` after its park, `new
            // InterruptedException()` with no message
            // (`VmConfig::virtual_sleep_continuation_shape`); the capture
            // lists that throw's frames (`JvmThread::virtual_sleep_hint`).
            {
                crate::vm::vm_exec::preload_virtual_thread_class(shared);
                thread.virtual_sleep_hint = Some(true);
                None
            }
        }
        VtYield::KeyedWait { key } => {
            if !thread
                .interrupted
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return RemountAction::Run;
            }
            shared
                .threads
                .virtual_thread_manager
                .cancel_wait_on_key(key, thread.thread_id.0);
            // No permit is given back here, unlike a sleep: the keyed wait
            // stands for `CountDownLatch.await`, whose AQS park consumes the
            // interrupt's `Parker` permit in HotSpot.
            None
        }
    };
    // Cleared as `NativeContextImpl::is_interrupted(true)` clears it, which is
    // what the platform-thread sleep natives do before they throw.
    // SBR wave 6 (lane `thread`): and the Java `interrupted` field, which the
    // retired `Thread.isInterrupted()` reads. nio21 wave 5 (L1): both under
    // the interrupt lock, so a racing `interrupt()` cannot split them.
    crate::vm::vm_exec::consume_thread_interrupt(
        shared,
        &thread.interrupted,
        thread.java_thread_obj,
    );
    match super::exceptions::create_exception_object(
        shared,
        thread,
        "java/lang/InterruptedException",
        message,
    ) {
        Ok(exc) => RemountAction::Throw(exc),
        Err(e) => {
            // Wave 48 (lane L1): what failed, for
            // `interpreter-L1-a-virtual-thread-sleep-interrupt-has-the-platform-shape`.
            if crate::runtime::env_cache::frame_trace() {
                eprintln!(
                    "[VT_REMOUNT_THROW] tid={} not built: {e:?}",
                    thread.thread_id.0
                );
            }
            RemountAction::Fail(e)
        }
    }
}

/// Record the return conversion a lambda dispatch owes its caller when a
/// continuation yield has just crossed the nested Rust call it entered the
/// impl method through (interpreter round i1 wave 25, lane L7; see
/// [`crate::threading::jvm_thread::ContinuationReturnAdapter`]).
///
/// `frame_index` is `thread.frames.len()` when the dispatch began, so the
/// frame there is the first one the dispatch pushed -- the impl method's;
/// `call_descriptor` is the SAM call's descriptor. Nothing is recorded when
/// the dispatch pushed no frame (a native impl yielded directly: its caller
/// is the leaf and resumes past its invoke with nothing to receive), or when
/// the impl's return reaches the caller unchanged.
///
/// Contract: the record is consumed by the value-return arms of the dispatch
/// loop through
/// [`push_return_across_continuation_boundary`], and an entry whose frame has
/// left by any other route (an exception, a `void` return) is dropped by the
/// next [`prune_continuation_return_adapters`].
pub(crate) fn note_continuation_return_boundary(
    thread: &mut JvmThread,
    frame_index: usize,
    call_descriptor: &str,
) {
    let (frame_seq, caller_return) = {
        let Some(frame) = thread.frames.get(frame_index) else {
            return;
        };
        let caller_return = descriptor_return_ref(call_descriptor);
        let callee_return = descriptor_return_ref(frame.method_descriptor());
        if !continuation_return_needs_conversion(caller_return, callee_return) {
            return;
        }
        (frame.seq, Box::<str>::from(caller_return))
    };
    prune_continuation_return_adapters(thread);
    thread.continuation_return_adapters.push(
        crate::threading::jvm_thread::ContinuationReturnAdapter {
            frame_index,
            frame_seq,
            caller_return,
        },
    );
}

/// Whether a value returned with descriptor token `callee` must be converted
/// before a call site expecting `caller` may receive it: `coerce_return`'s
/// identity cases (the same token, two reference types, or two `void`s) need
/// nothing.
pub(in crate::runtime::interpreter) fn continuation_return_needs_conversion(caller: &str, callee: &str) -> bool {
    if caller == callee {
        return false;
    }
    let is_reference = |t: &str| t.starts_with('L') || t.starts_with('[');
    !(is_reference(caller) && is_reference(callee))
}

/// Drop every adapter whose frame is no longer on the stack at its index.
pub(in crate::runtime::interpreter) fn prune_continuation_return_adapters(thread: &mut JvmThread) {
    let frames = &thread.frames;
    thread.continuation_return_adapters.retain(|a| {
        frames
            .get(a.frame_index)
            .is_some_and(|f| f.seq == a.frame_seq)
    });
}

/// The value-return arms' push into the parent frame while the thread owes a
/// continuation return conversion (`continuation_return_adapters` is not
/// empty; wave 25, lane L7). Frame `frame_idx` is returning `value` (`raw`:
/// the fast arm's opcode and bit-exact slot, `None` from the decoded arm) and
/// is still on the stack. With an adapter for it the value is converted as the
/// lambda door's `coerce_return` would have converted it; otherwise it is
/// pushed exactly as the arm itself pushes it.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn push_return_across_continuation_boundary(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    value: Value,
    raw: Option<(u8, CompactValue)>,
) -> Result<(), MethodCallFailed> {
    prune_continuation_return_adapters(thread);
    let adapter = thread
        .continuation_return_adapters
        .iter()
        .position(|a| a.frame_index == frame_idx)
        .map(|pos| thread.continuation_return_adapters.swap_remove(pos));
    let converted = match adapter {
        Some(adapter) => {
            let callee_return: Box<str> = match thread.frames.get(frame_idx) {
                Some(frame) => descriptor_return_ref(frame.method_descriptor()).into(),
                None => Box::from(""),
            };
            Some(coerce_return(
                shared,
                thread,
                &adapter.caller_return,
                &callee_return,
                Some(value),
            )?)
        }
        None => None,
    };
    let Some(parent) = frame_idx
        .checked_sub(1)
        .and_then(|p| thread.frames.get_mut(p))
    else {
        return Ok(());
    };
    parent.exec_epoch = parent.exec_epoch.wrapping_add(1);
    let push = match (converted, raw) {
        // A `void` SAM: the impl's value is dropped.
        (Some(None), _) => return Ok(()),
        (Some(Some(v)), _) => v,
        (None, Some((opcode, cv))) => {
            push_fast_return_value(&mut parent.stack, opcode, cv, value);
            return Ok(());
        }
        (None, None) => value,
    };
    push_invoke_return_value(&mut parent.stack, push)
        .map_err(|e| MethodCallFailed::InternalError(VmError::Runtime(e)))
}

/// The `void`-return arms' twin of [`push_return_across_continuation_boundary`]
/// (interpreter round i6 wave 115, lane L4; the `V -> reference` half of
/// `i26-L4-proposal-let-a-virtual-thread-unmount-under-the-method-handle-natives`):
/// frame `frame_idx` returns `void` (still on the stack) while the thread owes
/// continuation return conversions. An adapter recorded for it whose caller
/// return is a reference is the door's `null` -- a method-handle door that ran
/// the `void` leaf transparent (`lang_invoke::tail_leaf_conversion`,
/// `CRATONVM_MH_VOID_TAIL_UNMOUNT`) and was unwound by the yield -- so `null`
/// is pushed into the parent, as the door would have returned it. Any other
/// adapter for the frame is dropped (nothing records one: a lambda door never
/// owes a `void` impl a value, `continuation_return_needs_conversion`), and
/// with no adapter for it nothing is pushed: the arm's own `void` return.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn push_void_return_across_continuation_boundary(
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<(), MethodCallFailed> {
    prune_continuation_return_adapters(thread);
    let Some(pos) = thread
        .continuation_return_adapters
        .iter()
        .position(|a| a.frame_index == frame_idx)
    else {
        return Ok(());
    };
    let adapter = thread.continuation_return_adapters.swap_remove(pos);
    if !(adapter.caller_return.starts_with('L') || adapter.caller_return.starts_with('[')) {
        return Ok(());
    }
    let Some(parent) = frame_idx
        .checked_sub(1)
        .and_then(|p| thread.frames.get_mut(p))
    else {
        return Ok(());
    };
    parent.exec_epoch = parent.exec_epoch.wrapping_add(1);
    push_invoke_return_value(&mut parent.stack, Value::Object(None))
        .map_err(|e| MethodCallFailed::InternalError(VmError::Runtime(e)))
}


/// jit-invokedynamic-groovy-regression fix — run an already-materialized
/// interpreter frame (e.g. one reconstructed from a JIT deopt snapshot, with
/// its `pc` mid-method and operand stack pre-populated) to completion on this
/// thread, returning the method result exactly like [`execute`].
///
/// This is the dispatch-helper precise-resume primitive
/// (`try_resume_trapped_callee`, vm/src/jit/helpers.rs): a compiled callee
/// that hit its unconditional `invokedynamic` uncommon trap left a precise
/// frame snapshot; resuming that frame HERE — at the helper call site, before
/// any compiled caller's epilogue bail — completes the callee in the
/// interpreter and hands the real result back to the compiled caller, so no
/// side effect is re-run and no sentinel escapes.
///
/// Mirrors [`execute`]'s tail exactly: push + entry hooks, STW safepoint
/// check, panic-protected `execute_frame`, orphaned-inner-frame truncation,
/// pop + recycle. See `execute`'s own comments for why each step exists.
pub(crate) fn execute_prebuilt_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame: Frame,
) -> MethodCallResult {
    let frames_depth_before_push = thread.frames.len();
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[FRAME_PUSH/execute_prebuilt_frame] depth={} {}.{}{} pc={}",
            thread.frames.len(),
            frame.class_name(),
            frame.method_name(),
            frame.method_descriptor(),
            frame.pc
        );
    }
    push_frame_and_fire_entry(shared.vm_identity, thread, frame);
    gci_i11jd_witness::note_entry(shared, thread);
    run_pushed_frame_to_completion(shared, thread, frames_depth_before_push)
}

/// [`execute_prebuilt_frame`] for a caller that holds the callee's
/// `CachedBytecodeMethod` and its decoded arguments rather than a `Frame` it
/// built itself.
///
/// # Why this exists
///
/// `execute_prebuilt_frame` takes a `Frame` by value and hands it to
/// `push_frame_and_fire_entry`, which **harvests and trims the retired slot**
/// before pushing. So a caller that builds its own frame does not merely miss
/// the in-place install — it destroys the slot the next interpreted call at
/// that depth would have rebuilt itself in, once per call. That is the same
/// shape the general dispatchers were in before `install_cached_frame`, where
/// it measured ~70 cycles a call, and the JIT's three interpreted-callee
/// helpers (`jit::helpers`' static / virtual / special bytecode callees) were
/// the population left in it: every JIT-to-interpreter call undid frame-slot
/// reuse for the interpreter frames beneath it.
///
/// Nothing about the callee's execution changes — `install_cached_frame`
/// produces a frame indistinguishable field for field from
/// `Frame::new_pooled_cached`'s (four unit tests in `runtime::frame` assert
/// exactly that across all three install shapes), and
/// `run_pushed_frame_to_completion` is `execute_prebuilt_frame`'s own tail.
pub(crate) fn install_and_run_cached_frame(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    trace_tag: Option<&str>,
) -> MethodCallResult {
    let frames_depth_before_push = thread.frames.len();
    install_cached_frame_nested(shared, thread, cached, args, None, trace_tag, false);
    run_pushed_frame_to_completion(shared, thread, frames_depth_before_push)
}

/// [`install_and_run_cached_frame`] for the lambda door, which stands for the
/// spun proxy class's method (interpreter round i7 wave 137, lane L1;
/// `VmConfig::jvmti_lambda_entry_pop`, `CRATONVM_JVMTI_LAMBDA_ENTRY_POP`,
/// parity, default ON, `=0` restores `OPAQUE_FRAME`;
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// On HotSpot the proxy's method is a Java frame under the target, so a C
/// agent's `PopFrame` of the target from its `MethodEntry` callback is served:
/// the target is popped, the proxy's invoke runs again, and `MethodEntry` is
/// posted again (MEASURED, `L1W134JvmtiBlockedEntryFrameControl` row
/// `entry lambda pop`: `pop=0 hits=2 calls=1`). Here the target's frame is
/// the entry frame of the loop this door runs, so its caller is no frame of
/// that loop. While a C agent listens for `MethodEntry` (and the frame-control
/// switches are on), the push is marked ([`ReinvokableEntryScope`]), the
/// callback's point admits a `PopFrame` of it, and the door takes the request
/// before the frame runs (`debug::pop_frames::take_reinvokable_entry_pop`):
/// the frame is popped unrun and the target called again with the arguments
/// its parameter slots hold, which is what re-running the proxy's forwarding
/// invoke does. A `ForceEarlyReturn*` made after the `PopFrame` is returned
/// by the door, as the proxy's frame returns it on HotSpot. Without a
/// listener: one relaxed load more than [`install_and_run_cached_frame`].
#[inline]
pub(crate) fn install_and_run_cached_frame_reinvokable(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    trace_tag: Option<&str>,
) -> MethodCallResult {
    // Interpreter round i7 wave 139, lane L1 (`VmConfig::jdwp_lambda_target_pop`):
    // while the proxy's row is listed, a JDWP `PopFrames` of the frame pushed
    // here is served (`debug::pop_frames::try_door_entry_pop`). Held for the
    // whole run, either arm below.
    #[cfg(feature = "experimental-debug")]
    let _door = (shared.debug.debugger_gates.c_lambda_frames_armed())
    .then(|| crate::debug::pop_frames::DoorEntryScope::enter(thread.frames.len() + 1));
    #[cfg(feature = "experimental-debug")]
    {
        if crate::runtime::jvmti::any_method_entry_listener_active()
        {
            return install_and_run_reinvoking(shared, thread, cached, args, trace_tag, false);
        }
    }
    install_and_run_cached_frame(shared, thread, cached, args, trace_tag)
}

/// [`install_and_run_cached_frame`] for the JIT helpers' interpreted callee,
/// whose caller is a compiled frame (interpreter round i7 wave 138, lane L1;
/// `VmConfig::jvmti_compiled_caller_entry_pop`,
/// `CRATONVM_JVMTI_COMPILED_CALLER_ENTRY_POP`, parity, default ON, `=0`
/// restores `OPAQUE_FRAME`; the residual of
/// `docs/feature-designs/real-jdk-mode.md`).
///
/// HotSpot serves a C agent's `PopFrame` of such a callee from its
/// `MethodEntry` callback: the caller is a Java frame, which it deoptimizes,
/// and the interpreter re-runs the caller's invoke (`JvmtiEnv::PopFrame`
/// admits any Java caller; only a native or call-stub caller is
/// `OPAQUE_FRAME`). Here the callee is the entry frame of the loop this door
/// runs, so the door does what re-running the invoke does: it calls the
/// same resolved callee again, with the arguments the popped frame's
/// parameter slots hold ([`install_and_run_cached_frame_reinvokable`]'s
/// mechanism). The compiled caller stays compiled; nothing an agent reads
/// tells the two apart except `IsMethodCompiled`-style questions.
///
/// Two requests are not served that way, and are dropped untaken (the frame
/// runs, as for a suspended thread's dropped pop): a `ForceEarlyReturn*` made
/// after the `PopFrame` in the same callback (the value is the compiled
/// caller's to return, which this door cannot make it do; it is moved to the
/// callee, `early_return::rekey_jvmti_forced_after_dropped_pop`), and a pop
/// whose re-read receiver is of another class than the call's (a `SetLocal`
/// of slot 0; HotSpot's re-run invoke would dispatch again, which this door,
/// holding the resolved callee, cannot) -- `pop_frames::keep_compiled_caller_entry_pop`.
/// Without a `MethodEntry` listener: one relaxed load more than
/// [`install_and_run_cached_frame`].
#[inline]
pub(crate) fn install_and_run_cached_frame_from_compiled(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    trace_tag: Option<&str>,
) -> MethodCallResult {
    // Interpreter round i7 wave 139, lane L1 (`VmConfig::jdwp_compiled_caller_pop`):
    // while a JDWP request is in force, a `PopFrames` of the frame pushed
    // here is served (`debug::pop_frames::try_door_entry_pop`). One relaxed
    // load otherwise.
    #[cfg(feature = "experimental-debug")]
    let _door = (shared
        .debug
        .breakpoints_active
        .load(std::sync::atomic::Ordering::Relaxed))
        .then(|| crate::debug::pop_frames::DoorEntryScope::enter_compiled_caller(thread.frames.len() + 1));
    #[cfg(feature = "experimental-debug")]
    {
        if crate::runtime::jvmti::any_method_entry_listener_active()
        {
            return install_and_run_reinvoking(shared, thread, cached, args, trace_tag, true);
        }
    }
    install_and_run_cached_frame(shared, thread, cached, args, trace_tag)
}

/// The body of [`install_and_run_cached_frame_reinvokable`] (and, with
/// `compiled_caller`, of [`install_and_run_cached_frame_from_compiled`];
/// wave 138) while a C agent listens for `MethodEntry`: push, take a
/// `PopFrame` the entry callback made, and push again until the frame is left
/// to run.
#[cfg(feature = "experimental-debug")]
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn install_and_run_reinvoking(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    trace_tag: Option<&str>,
    compiled_caller: bool,
) -> MethodCallResult {
    use crate::debug::pop_frames::ReinvokeStep;
    let is_static = cached.is_static;
    // Wave 138: the receiver's class, which a re-invoke from a compiled
    // caller must find unchanged (read before any push: no collection yet,
    // and a moving one keeps the class anyway).
    let receiver_class = match args.first() {
        Some(Value::Object(Some(receiver))) if compiled_caller && !is_static => {
            Some(shared.mem.heap.class_id_of(*receiver))
        }
        _ => None,
    };
    let mut reread: Option<Vec<Value>> = None;
    loop {
        let frames_depth_before_push = thread.frames.len();
        {
            let current: &[Value] = reread.as_deref().unwrap_or(args);
            let _mark = ReinvokableEntryScope::enter(frames_depth_before_push + 1);
            install_cached_frame_nested(
                shared,
                thread,
                Arc::clone(&cached),
                current,
                None,
                trace_tag,
                false,
            );
        }
        if compiled_caller {
            crate::debug::pop_frames::keep_compiled_caller_entry_pop(
                shared,
                thread,
                frames_depth_before_push + 1,
                receiver_class,
            );
        }
        match crate::debug::pop_frames::take_reinvokable_entry_pop(
            shared,
            thread,
            frames_depth_before_push + 1,
            is_static,
        ) {
            ReinvokeStep::Run => {
                return run_pushed_frame_to_completion(shared, thread, frames_depth_before_push)
            }
            ReinvokeStep::Return(value) => return Ok(value),
            ReinvokeStep::Reinvoke(next) => reread = Some(next),
        }
    }
}

/// [`install_and_run_cached_frame`] for a template that may be an
/// `ACC_SYNCHRONIZED` method (see `build_bytecode_callee_cached`).
///
/// The monitor is taken the way the interpreter's own fast doors take it —
/// `invoke_fast::door_monitor_acquire`, an UNCONTENDED acquire only — and
/// handed to the frame as `monitor_on_exit`, which
/// `pop_and_recycle_frame_with_reason` releases however the frame leaves:
/// return, exception unwind, or the orphan sweep in
/// [`run_pushed_frame_to_completion`].
///
/// `None` means "decline, nothing was done": contention (the general path owns
/// the blocking, GC-pinning acquire), `CRATONVM_JIT_NO_DOOR_SYNC`, a receiver
/// that is not an object, or a STATIC method whose class mirror does not exist
/// yet. The acquire is the LAST fallible step, so a monitor that was taken is
/// always installed on a frame.
///
/// A static synchronized method locks its class mirror. Creating one can
/// allocate, which this door must not do, but once it exists it is a map read
/// (`class_mirrors`, the fast path of `get_or_create_class_mirror`), with no
/// safepoint between it and the acquire. The general path creates the mirror
/// on the first call it serves, so only that call declines (round 11 wave 5,
/// r11w4-sync-compiled-caller-cannot-call-a-synchronized-callee: a compiled
/// caller's `static synchronized` callee took the by-name route on every call).
///
/// Since round 12 wave 8 an INSTANCE template enters its published
/// wrapped-entry body the same way (`CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`,
/// default on; a four-field `synchronized int step()` was ~520 ns a call
/// interpreted). The history below is why it was interpreted before; the
/// measurement it cites priced a different door (a blocking acquire through
/// `execute_jit_call_oneshot`), not this uncontended adoption. Before that, an
/// INSTANCE template was interpreted even when the background tier had
/// published a wrapped-entry body for the method. Entering that body instead --
/// through `execute_jit_call_oneshot` with a `JitSynchronizedMonitorGuard` --
/// was built and MEASURED 2026-09-23 on `Throwable.fillInStackTrace()`, the hot
/// case, and came out ~5 % SLOWER on `ThrowOne new` in an interleaved same-host
/// A/B: the blocking acquire, the JIT-entry guard and one more compiled frame
/// for every stack walk cost more than interpreting a ten-bytecode body saves.
///
/// A STATIC template with a published wrapped body enters it (round 11 wave 6):
/// the door's uncontended hold is adopted by
/// `jit_bridge::execute_wrapped_body_door_locked`, so there is no blocking
/// acquire, and the interpreted alternative is a body of `getstatic` /
/// `putstatic`, each a full field-ref resolution in the interpreter. A
/// compiled attempt that cannot finish hands the hold back and the template
/// runs under it.
pub(crate) fn install_and_run_cached_frame_monitored(
    shared: &SharedVm,
    thread: &mut JvmThread,
    cached: Arc<CachedBytecodeMethod>,
    args: &[Value],
    trace_tag: Option<&str>,
) -> Option<MethodCallResult> {
    if !cached.is_synchronized {
        // Interpreter round i7 wave 138, lane L1: a compiled caller's callee
        // whose `MethodEntry` callback pops it is called again.
        return Some(install_and_run_cached_frame_from_compiled(
            shared, thread, cached, args, trace_tag,
        ));
    }
    let lock_object = if cached.is_static {
        let mirror = shared
            .classes
            .class_mirrors
            .read()
            .get(&cached.declaring_class_id)
            .copied();
        mirror?
    } else {
        match args.first() {
            Some(Value::Object(Some(r))) => *r,
            _ => return None,
        }
    };
    let monitor = invoke_fast::door_monitor_acquire(shared, thread, &cached, Some(lock_object))?;
    // A template whose wrapped-entry body is published: enter the compiled
    // body under the hold just taken instead of interpreting (static since
    // round 11 wave 6, instance since round 12 wave 8 under
    // `CRATONVM_JIT_DOOR_SYNC_INSTANCE_BODY`; see
    // `jit_bridge::execute_wrapped_body_door_locked` for the cost).
    if let Some(lock) = monitor {
        if let Some(compiled) =
            jit_bridge::door_locked_wrapped_body(shared, thread, &cached, args.len())
        {
            match jit_bridge::execute_wrapped_body_door_locked(
                shared, thread, &compiled, &cached, args, lock,
            ) {
                jit_bridge::DoorLockedOutcome::Finished(result) => return Some(result),
                // The compiled attempt could not finish: re-run interpreted
                // under the SAME hold, from the relocated arguments.
                jit_bridge::DoorLockedOutcome::Interpret {
                    monitor,
                    args: relocated,
                } => {
                    let frames_depth_before_push = thread.frames.len();
                    install_cached_frame_nested(
                        shared,
                        thread,
                        cached,
                        &relocated,
                        Some(monitor),
                        trace_tag,
                        false,
                    );
                    return Some(run_pushed_frame_to_completion(
                        shared,
                        thread,
                        frames_depth_before_push,
                    ));
                }
            }
        }
    }
    let frames_depth_before_push = thread.frames.len();
    install_cached_frame_nested(shared, thread, cached, args, monitor, trace_tag, false);
    Some(run_pushed_frame_to_completion(
        shared,
        thread,
        frames_depth_before_push,
    ))
}

/// The tail of [`execute_prebuilt_frame`], for a caller that has ALREADY
/// pushed the frame it wants run.
///
/// `frames_depth_before_push` is the depth `thread.frames` had before that
/// push, so the orphaned-inner-frame truncation and the final pop can restore
/// it exactly. Split out for the deopt-resume / exception-routing sinks, which
/// build and push their frame themselves and then need it run to completion
/// synchronously rather than handed to the interpreter's stepping loop — see
/// `jit_bridge::execute_jit_call_oneshot`, whose whole reason for existing is
/// that a one-shot dispatch helper has no such loop to hand it to.
pub(crate) fn run_pushed_frame_to_completion(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frames_depth_before_push: usize,
) -> MethodCallResult {
    debug_assert!(thread.frames.len() > frames_depth_before_push);
    // Interpreter round i2 wave 53, lane L6: as in `execute` (one load).
    let _nested_java_ack = NestedJavaAck::enter(thread);
    if shared
        .mem
        .gc_barrier
        .stw_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint_check(shared, thread);
    }
    // Wave 133 (lane L4): the native stack guard `execute` asks, for the doors'
    // pushed-frame entry, which no `EXEC_DEPTH` level counts. The frame never
    // ran, so none of its handlers applies (HotSpot raises the error at the
    // callee's entry bang); popping it is the exception unwind's.
    //
    // Wave 134 (lane L4, review of w133 item 3):
    // * Only a frame that has not run trips (`reentry_trip_applies`): one
    //   pushed frame at bytecode index 0. A deopt resume pushes frames that
    //   already ran compiled (a resumed chain, a frame mid-body); popping
    //   those skipped their handlers and dropped their held state, so they
    //   run on, and the next fresh nested entry trips instead.
    // * Every frame reaching here holds its monitor or needs none: the
    //   doors take an uncontended acquire only (`door_monitor_acquire`), the
    //   compiled doors decline a contended one under
    //   `CRATONVM_SYNC_METHOD_FRAME_FIRST` (`frame_first_door_try`), and the
    //   only `SyncEntry::Deferred` push is `execute`'s own, whose guard runs
    //   before it. A held `monitor_on_exit` is released by the pop.
    // * A deferred `MethodEntry` record naming a frame at or above the push
    //   is dropped, as the frame-limit trip in `invoke.rs` drops it.
    // * The frames are popped as an exception unwind (`MethodExit` and
    //   `FramePop` with `was_popped_by_exception`), pairing the `MethodEntry`
    //   the push already posted; a `NotifyFramePop` an agent requested from
    //   that `MethodEntry` is consumed by the pop, not left pending.
    // The error is still built in place here: these callers include the
    // JIT's one-shot resumes, for which an internal error is a re-run
    // request, not an exception to convert.
    if reentry_trip_applies(thread, frames_depth_before_push) && reentry_native_stack_exhausted() {
        if thread.deferred_method_entry_depth > frames_depth_before_push {
            thread.deferred_method_entry_depth = 0;
        }
        // Interpreter round i7 wave 136 (lane L1): a `ForceEarlyReturn*` the
        // pushed frame's `MethodEntry` callback recorded at its entry-frame
        // callback point (`CRATONVM_JVMTI_NESTED_ENTRY_FRAME_CONTROL`) is the
        // unrun frame's: dropped with it, before the error's constructor runs
        // Java, so it never stands for a later frame at the same count.
        drop_frame_control_of_unrun_frames(shared, thread, frames_depth_before_push);
        exception_dispatch::withdraw_at_pushed_ctor_entry_failure(
            shared,
            thread,
            frames_depth_before_push,
        );
        let mut result = Err(raise_reentry_stack_overflow(shared, thread));
        pop_unrun_frames_by_exception_keeping_result(
            shared,
            thread,
            frames_depth_before_push,
            &mut result,
        );
        return result;
    }

    // Round 13 wave 6 (lane chain2): the floor is the OUTERMOST pushed frame,
    // so a resumed inlined chain's exception reaches its callers' handlers and
    // its callee's return continues the caller's body. For one pushed frame it
    // is the same index `execute_frame` takes.
    // `CRATONVM_DEOPT_CHAIN_RUN_FROM_OUTERMOST=0` restores the top-frame floor.
    //
    // This runs on every nested interpreter entry (every lambda/door call that
    // runs a pushed frame), and an uncached declared-flag read costs a UTF-8
    // decode, two name hashes and an allocation per call. gci i14/sx: read
    // once per VM (`SharedVm::deopt_chain_run_from_outermost`, at
    // construction) instead of nio21 wave 16's process `OnceLock`, which tests
    // had to bypass; a test now builds its VM under `with_thread_overrides`.
    let from_outermost = shared.deopt_chain_run_from_outermost;
    let pin_floor = thread.native_pin_roots.len();
    let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if from_outermost {
            execute_frame_from_index(shared, thread, frames_depth_before_push)
        } else {
            execute_frame(shared, thread)
        }
    })) {
        Ok(r) => r,
        // The heap-exhaustion unwind is an `OutOfMemoryError`, not a panic;
        // see the same arm in `execute` (gc-common w5-c).
        Err(panic_info)
            if crate::runtime::native_oom::is_native_alloc_oom_payload(&*panic_info) =>
        {
            thread.native_pin_roots.truncate(pin_floor);
            Err(crate::runtime::native_oom::caught_oom_as_call_failure(
                panic_info,
            ))
        }
        Err(panic_info) => {
            let msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_info.downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic in bytecode execution".to_string()
            };
            if let Some(f) = thread.frames.last() {
                eprintln!(
                    "[PANIC_IN/prebuilt] {}.{}{} pc={} max_stack={} :: {}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    f.pc,
                    f.max_stack,
                    msg
                );
            }
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::NotImplemented { feature: msg },
            )))
        }
    };

    // A continuation yield keeps every frame, exactly as `execute`'s yield arm
    // does: the frames above `frames_depth_before_push` ARE the virtual
    // thread's continuation (interpreter round i1 wave 25, lane L7). Popping
    // them here -- this function used to fall through to the orphan sweep --
    // left the frame that made this nested call as the continuation's leaf, so
    // the remount resumed it past its invoke as if the callee had returned. A
    // bound method reference (`latch::await`, `queue::take`, served by the
    // lambda door's `try_invoke_cached_lambda_impl` from its first call) or a
    // static lambda body already run once (`lambda_global_impl_owner`) then
    // "returned" from a wait whose count never reached zero
    // (`L7W25ContinuationBoundaries.java`).
    if matches!(
        result,
        Err(MethodCallFailed::InternalError(
            VmError::ContinuationYield { .. }
        ))
    ) {
        return result;
    }

    // Truncate any orphaned inner frames execute_frame may have left, then pop
    // our own frame — same accounting (and the same result pin) as `execute`.
    let mut result = result;
    pop_root_frames_keeping_result(shared, thread, frames_depth_before_push, &mut result);

    result
}

// ---------------------------------------------------------------------------
// PGO profiling helpers
// ---------------------------------------------------------------------------

/// Build a `MethodKey` from a frame, used to key into the `ProfileStore`.
///
/// Called only at branch/invoke sites during warmup — the Arc clones are
/// acceptable overhead before JIT compilation takes over.
///
/// AUDIT CRIT-3 fix: removed `#[inline(never)]`.  In the hot-loop branch
/// sites this is guarded by `jit_profile::is_profiling_enabled()`, so it
/// only executes during active warmup; allowing inlining lets LLVM CSE
/// the two `Arc<str>` clones across adjacent record_branch/record_backedge
/// pairs (e.g. `if_icmp*` + back-edge `record_backedge`).
///
/// LOW — takes the frame by ref and uses the ref-returning
/// `method_name_arc_ref` / `method_descriptor_arc_ref` accessors to
/// hold off on the `Arc::clone` refcount bumps until the `MethodKey`
/// is actually being constructed (the call site can elide the call
/// entirely when profiling is disabled). Previously each call did 2
/// `Arc::clone` atomic ops up-front in the accessor, plus 2 more
/// implicit clones into the MethodKey fields.
#[inline]
pub(in crate::runtime::interpreter) fn make_method_key(frame: &crate::runtime::frame::Frame) -> MethodKey {
    MethodKey {
        class_id: frame.class_id.as_u32(),
        method_name: Arc::clone(frame.method_name_arc_ref()),
        descriptor: Arc::clone(frame.method_descriptor_arc_ref()),
    }
}

/// PERF (round-5 vm #7): borrowed-key variant of `make_method_key` used by
/// the hot PGO recording paths.  Returns the three key components by value
/// (`u32`) and reference (`&Arc<str>`), letting the `ProfileStore`
/// `record_*_borrowed` family skip the two `Arc::clone` atomic bumps that
/// `MethodKey` construction otherwise requires per branch / back-edge.
#[inline]
pub(in crate::runtime::interpreter) fn method_key_parts<'a>(
    frame: &'a crate::runtime::frame::Frame,
) -> (u32, &'a Arc<str>, &'a Arc<str>) {
    (
        frame.class_id.as_u32(),
        frame.method_name_arc_ref(),
        frame.method_descriptor_arc_ref(),
    )
}

/// Per-thread handle cache for [`cratonvm_jit::profile::BranchCounters`].
///
/// # Why this exists
///
/// Recording one branch used to cost a 64-bit fingerprint of
/// `(class_id, name, descriptor)`, a shard `RwLock` read, an index probe, a
/// `MethodKey` comparison, a `parking_lot::Mutex` acquire and an `FxHashMap`
/// entry — on the interpreter's hot path, per branch. That price is the whole
/// reason `CRATONVM_TIER_PGO` has never shipped on, and the reason the
/// optimizing tier's scheduler reads an empty `branch_counts` at essentially
/// every compile.
///
/// The counters themselves are lock-free and indexed by pc; all that is left is
/// FINDING them, and a method's counters do not change while it runs. So this
/// caches the handle against the identity of the frame's
/// `CachedBytecodeMethod`. In a loop the whole lookup collapses to one pointer
/// comparison and one relaxed `fetch_add`.
///
/// # Why the method `Arc` is held, not just its address
///
/// The obvious key is `Arc::as_ptr(cm) as usize`. It is wrong on its own: a
/// `CachedBytecodeMethod` that is dropped and a new one allocated at the same
/// address would be served the previous method's counters — an ABA that would
/// mis-attribute a profile silently and never fail a test. Holding the `Arc`
/// keeps the address from being reused while the entry is live, so
/// `Arc::ptr_eq` is a real identity test.
///
/// Direct-mapped over eight ways rather than a single cell: a caller alternating
/// between two hot methods would otherwise thrash, and eight entries of two
/// `Arc`s each is nothing per thread.
pub(in crate::runtime::interpreter) const BRANCH_MEMO_WAYS: usize = 8;

thread_local! {
    pub(in crate::runtime::interpreter) static BRANCH_COUNTER_MEMO: std::cell::RefCell<
        Vec<Option<(Arc<cratonvm_jit::CachedBytecodeMethod>,
                    Arc<cratonvm_jit::profile::BranchCounters>)>>,
    > = std::cell::RefCell::new(vec![None; BRANCH_MEMO_WAYS]);
}

/// Record one conditional-branch observation for `frame`.
///
/// Falls back to the map-based recorder for a frame that carries no
/// `CachedBytecodeMethod` — the launcher's `main` and some reflective entries —
/// so no observation is lost by shape. `MethodProfile::snapshot` folds the two
/// sources together, so a method recorded through both is still counted once
/// per observation.
///
/// `#[inline(never)]` since interpreter round i1 wave 27 (lane L7; it was
/// `#[inline]`): `cond_branch_arm!` is expanded eighteen times in
/// `execute_frame_from_index`, so an inlined recorder (a thread-local memo, a
/// `RefCell` borrow, an `Arc` clone and drop) was eighteen copies of code
/// that runs only while a profile is being taken, grown into the loop that
/// every other bytecode runs in. A recording branch pays one call more.
#[inline(never)]
pub(in crate::runtime::interpreter) fn record_branch_for_frame(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    pc: usize,
    taken: bool,
) {
    // A frame running an OBSOLETE body (a redefinition replaced it with
    // different bytecode) executes the old version's pcs, and both arms below
    // key the profile by name, so its observations would land in the new
    // version's profile (r11-tier-profile-replay-residuals item 2). The map
    // path cannot tell the versions apart at all, and the counters path only
    // by length. One bit test per recorded branch.
    if frame.runs_obsolete_method() {
        return;
    }
    let Some(cm) = frame.cached_method() else {
        // The frame's length lets the store refuse a frame of another version
        // of the method that has not been marked obsolete yet (round 12 wave
        // 3; `MethodProfile::admits_frame_code_len`).
        let (cid, mn, md) = method_key_parts(frame);
        shared
            .jit
            .profile_store
            .record_branch_borrowed(cid, mn, md, pc, taken, frame.code.len());
        return;
    };
    // Cast: an `Arc` address used only to pick a memo way; identity is settled
    // by `Arc::ptr_eq` below, never by this value.
    let way = (Arc::as_ptr(cm) as usize >> 4) % BRANCH_MEMO_WAYS;
    // The record is taken INSIDE the memo borrow, through the memo's own
    // handle (interpreter round i1 wave 28, lane L7). It used to clone the
    // handle out of the memo and drop it after the record: two atomic
    // read-modify-writes on the counters' shared refcount per recorded branch,
    // beside the one the record itself is. `BranchCounters::record` is a
    // relaxed `fetch_update` on the counter cell and never re-enters this memo.
    BRANCH_COUNTER_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((have, counters)) = memo.get(way).and_then(|e| e.as_ref()) {
            if Arc::ptr_eq(have, cm) {
                counters.record(pc, taken);
                return;
            }
        }
        // The real length, not the padded `cm.code.len()`: the store stamps
        // it and a profile replay compares it with the class file's.
        let fresh = shared.jit.profile_store.branch_counters_borrowed(
            frame.class_id.as_u32(),
            frame.method_name_arc_ref(),
            frame.method_descriptor_arc_ref(),
            jit_bridge::cached_method_code_len(cm),
        );
        fresh.record(pc, taken);
        if let Some(slot) = memo.get_mut(way) {
            *slot = Some((Arc::clone(cm), fresh));
        }
    });
}

/// Record one execution of the invoke instruction at `pc` of `frame`
/// (round nio20 wave 2, lane L3; page
/// `r20-per-site-invoke-counts-are-recorded-on-slow-paths-only-FIXED-20261007.md`).
///
/// The four invoke arms of `execute_frame_from_index` call it under the
/// branch profile's own gate (`pgo_enabled || branch_profile_on`), before
/// they dispatch, so every execution of a site is counted once whatever door
/// answers it. Same memo, same handle and same obsolete-body rule as
/// [`record_branch_for_frame`]; a frame without a `CachedBytecodeMethod`
/// records nothing here (its slow-path rows still do). Kill switch
/// `CRATONVM_JIT_PROFILE_FAST_CALL_COUNTS=0`, read once per interpreter entry.
#[inline(never)]
pub(in crate::runtime::interpreter) fn record_call_for_frame(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    pc: usize,
) {
    if frame.runs_obsolete_method() {
        return;
    }
    let Some(cm) = frame.cached_method() else {
        return;
    };
    // Cast: an `Arc` address used only to pick a memo way (see
    // `record_branch_for_frame`).
    let way = (Arc::as_ptr(cm) as usize >> 4) % BRANCH_MEMO_WAYS;
    BRANCH_COUNTER_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((have, counters)) = memo.get(way).and_then(|e| e.as_ref()) {
            if Arc::ptr_eq(have, cm) {
                counters.record_call(pc);
                return;
            }
        }
        let fresh = shared.jit.profile_store.branch_counters_borrowed(
            frame.class_id.as_u32(),
            frame.method_name_arc_ref(),
            frame.method_descriptor_arc_ref(),
            jit_bridge::cached_method_code_len(cm),
        );
        fresh.record_call(pc);
        if let Some(slot) = memo.get_mut(way) {
            *slot = Some((Arc::clone(cm), fresh));
        }
    });
}

thread_local! {
    /// Per-thread memo of [`cratonvm_jit::profile::LoopExtents`], keyed by the
    /// identity of `Frame::code` (the memo holds that `Arc`, so a freed body's
    /// address can never alias a live one). Same shape as
    /// [`BRANCH_COUNTER_MEMO`]; used only under `pgo_enabled`.
    pub(in crate::runtime::interpreter) static LOOP_EXTENTS_MEMO: std::cell::RefCell<
        Vec<Option<(Arc<[u8]>, Arc<cratonvm_jit::profile::LoopExtents>)>>,
    > = std::cell::RefCell::new(vec![None; BRANCH_MEMO_WAYS]);
}

/// Run `f` on the loop table of `frame`'s body: the thread's memo on a hit,
/// else the profile slot's copy (built there once per body), which is then
/// memoized.
///
/// `f` runs inside the memo's borrow, on the memo's own handle (interpreter
/// round i1 wave 28, lane L7): this used to return an `Arc` clone, two atomic
/// read-modify-writes on the table's shared refcount per recorded back edge or
/// loop exit. `f` must not re-enter this memo; both callers only record into
/// the profile store.
pub(in crate::runtime::interpreter) fn with_loop_extents_for_frame<R>(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    f: impl FnOnce(&cratonvm_jit::profile::LoopExtents) -> R,
) -> R {
    let code = &frame.code;
    // Cast: an `Arc` address used only to pick a memo way; identity is settled
    // by `Arc::ptr_eq` below, never by this value.
    let way = (Arc::as_ptr(code) as *const u8 as usize >> 4) % BRANCH_MEMO_WAYS;
    LOOP_EXTENTS_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((have, extents)) = memo.get(way).and_then(|e| e.as_ref()) {
            if Arc::ptr_eq(have, code) {
                return f(&**extents);
            }
        }
        let (cid, mn, md) = method_key_parts(frame);
        let fresh = shared
            .jit
            .profile_store
            .loop_extents_borrowed(cid, mn, md, code);
        let answer = f(&*fresh);
        if let Some(slot) = memo.get_mut(way) {
            *slot = Some((Arc::clone(code), fresh));
        }
        answer
    })
}

/// Record one back edge taken by the branch at `branch_pc`, under its loop's
/// key ([`cratonvm_jit::profile::LoopExtents::back_edge_key`]: the header's
/// last back edge, so a `continue`'s `goto` counts toward the loop it
/// continues). Not an obsolete body's (see [`record_branch_for_frame`]).
///
/// The dispatch loop's back-edge sites (`cond_branch_arm!`, `goto_arm!`, the
/// decoded path) call it only under the hoisted `pgo_enabled`
/// (`CRATONVM_TIER_PGO`, off by default); out of line and cold since
/// interpreter round i1 wave 27 (lane L7), so the twenty macro expansions each
/// carry one call instead of the key read and the recorder.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn record_backedge_for_frame(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    branch_pc: usize,
) {
    if frame.runs_obsolete_method() {
        return;
    }
    let key =
        with_loop_extents_for_frame(shared, frame, |extents| extents.back_edge_key(branch_pc));
    let (cid, mn, md) = method_key_parts(frame);
    shared
        .jit
        .profile_store
        .record_backedge_borrowed(cid, mn, md, key);
}

/// Record a loop exit for every loop the TAKEN forward branch at `branch_pc`
/// (a conditional, or a `goto`) leaves (round 11 wave 19 introduced the
/// producer, r11-tier-profile-replay-residuals item 6; round 12 wave 3 keyed
/// it by the loop, tier proposal W2-3).
///
/// javac closes every `for` / `while` with a backward `goto` to the header,
/// and a branch inside `[header, that goto]` whose target lies past the goto
/// leaves the loop: the header's own test, a `break`, a labelled `continue`
/// of an outer loop. The table ([`with_loop_extents_for_frame`]) answers which
/// loops, each keyed by the header's LAST back edge, the key the back-edge
/// recorder ([`record_backedge_for_frame`]) and the single-pass planner use.
/// It used to guess from the three bytes before the target, which charged
/// the `if` of `while (c) { if (x) { A; continue; } B; }` as an exit of a
/// "loop" keyed by the `continue`'s `goto`
/// (`CRATONVM_JIT_PGO_LOOP_KEYS=0` restores that rule).
///
/// A hint only: the unroll planner admits a loop from its own analysis.
/// Called only under the hoisted `pgo_enabled` gate
/// (`CRATONVM_TIER_PGO` / `CRATONVM_TIER_PGO_ALWAYS`), so a default run never
/// reaches it: `#[cold]` as well since interpreter round i1 wave 28 (lane L7),
/// like [`record_backedge_for_frame`], so the call at each of the twenty
/// branch-macro expansions is laid out off the path every other run takes
/// (round 12 added the `goto` arm's forward-exit call).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn record_loop_exit_for_frame(
    shared: &crate::vm::SharedVm,
    frame: &crate::runtime::frame::Frame,
    branch_pc: usize,
) {
    if frame.runs_obsolete_method() {
        return;
    }
    with_loop_extents_for_frame(shared, frame, |extents| {
        let keys = extents.exits_at(branch_pc);
        if keys.is_empty() {
            return;
        }
        let (cid, mn, md) = method_key_parts(frame);
        for &key in keys {
            let key = key as usize; // Cast: u32 -> usize widens
            shared
                .jit
                .profile_store
                .record_loop_exit_borrowed(cid, mn, md, key);
        }
    });
}

// ---------------------------------------------------------------------------
// Frame pop helper (releases synchronized monitor if present)
// ---------------------------------------------------------------------------

/// Pop the top frame from `thread.frames`, release its monitor (if any), and
/// recycle the frame's allocations.  This must be used in place of bare
/// `thread.frames.pop()` + `thread.recycle_frame()` whenever a frame pushed
/// via the stackless invoke path is being removed (return or exception unwind).
///
/// T10.7 — delegates to `JvmThread::recycle_frame_with_shared` so overflow
/// from the per-thread pool spills into the VM-wide `operand_stack_pool` /
/// `tag_pool` instead of being dropped on the floor.
#[inline]
pub fn pop_and_recycle_frame(shared: &SharedVm, thread: &mut JvmThread) {
    pop_and_recycle_frame_gated(shared, thread, loop_work_tierup_enabled, || {
        crate::runtime::env_cache::frame_trace() || invoke_phases::on()
    });
}

/// [`pop_and_recycle_frame`] with its two process gates supplied by the
/// caller: `loop_work_on` answers `CRATONVM_JIT_LOOP_WORK_TIERUP`
/// ([`loop_work_tierup_enabled`]) and `diagnostics_on` the frame trace or the
/// invoke-phase instrument; each is asked exactly where the plain-return test
/// asked it, so a caller passing the live reads has the old function. The
/// dispatch loop's return arms pass the values it hoisted at entry
/// (interpreter round i2 wave 61, lane L7), the `pgo_enabled` tradeoff: a
/// diagnostic armed mid-run is observed at the next entry. Until then every
/// interpreted return read two memo slots, and a returning frame that had
/// looped a `OnceLock` as well.
#[inline(always)]
pub(in crate::runtime::interpreter) fn pop_and_recycle_frame_gated(
    shared: &SharedVm,
    thread: &mut JvmThread,
    loop_work_on: impl FnOnce() -> bool,
    diagnostics_on: impl FnOnce() -> bool,
) {
    // A plain return (interpreter round i1 wave 37, lane L7): no monitor to
    // release, no `FramePop` owed, no loop work to credit, no diagnostic
    // armed. Everything `pop_and_recycle_frame_with_reason` would then do is
    // the caller's epoch bump and the retire, done here without the call,
    // so the value- and void-return arms of the dispatch loop no longer
    // depend on whether fat LTO inlines a 200-line function into them. Each
    // test below is one of the full function's own gates, in its order; a
    // new duty added there must add its gate here.
    if let Some(top) = thread.frames.last() {
        if top.held_monitors.is_empty()
            && top.monitor_on_exit.is_none()
            && thread.frame_pop_requests.is_empty()
            // A looping frame owes the loop-work credit, and OSRENTRY6-3's
            // per-method sum (`osr_early_request_armed`, asked only when the
            // credit is off: lane mon11, wave 11 -- this gate used to skip
            // that sum on every plain return, so `CRATONVM_JIT_OSR_EARLY_REQUEST`
            // was fed only by unwinds and outermost pops whenever
            // `CRATONVM_JIT_LOOP_WORK_TIERUP` was off).
            && (top.backward_count == 0 || (!loop_work_on() && !osr_early_request_armed()))
            && !diagnostics_on()
        {
            let depth = thread.frames.len();
            if depth >= 2 {
                // As in the full function: the caller re-executes from here.
                let caller = &mut thread.frames[depth - 2];
                caller.exec_epoch = caller.exec_epoch.wrapping_add(1);
            }
            thread.recycle_top_frame_in_place(
                &shared.mem.operand_stack_pool,
                &shared.mem.tag_pool,
                shared.config.frame_slot_reuse,
            );
            return;
        }
    }
    pop_and_recycle_frame_with_reason(shared, thread, /*was_popped_by_exception=*/ false);
}

/// Pop the top frame and return its storage to the per-thread / VM-wide
/// pools.  `was_popped_by_exception` controls whether the JVMTI `FramePop`
/// and `MethodExit` events fire with the abrupt-completion flag set.
///
/// The callers reach this via:
///   * Normal return (`pop_and_recycle_frame`) — `false`.
///   * Exception unwind through stackless / bytecode frames —
///     pass `true` directly.
///
/// Out of line (wave 37, lane L7): the plain return's work is inlined by
/// [`pop_and_recycle_frame`], and this body is the same call from every site
/// whatever the unit's inlining budget says.
#[inline(never)]
pub fn pop_and_recycle_frame_with_reason(
    shared: &SharedVm,
    thread: &mut JvmThread,
    was_popped_by_exception: bool,
) {
    // T17.Δ.2 — JVMTI MethodExit on exception unwind. Normal-return exits
    // are already fired from the return opcodes; here we handle only the
    // abrupt case. Cost when no agent is subscribed: single Acquire load.
    //
    // The guard is the process-wide union flag (over-approximates: it can be
    // true because a *different* VM has a MethodExit listener); the delivery
    // is `_for_vm`, which resolves this VM's environment and re-checks that
    // environment's own enable set. Guards may over-approximate, delivery
    // may not.
    if was_popped_by_exception && crate::runtime::jvmti::any_method_exit_listener_active() {
        if let Some(top) = thread.frames.last() {
            let method_id = jvmti_method_id(shared.vm_identity, top);
            let tid = thread.thread_id.0;
            // gci i11/ja: delivered under this thread's own handle.
            post_on_own_thread(thread, || {
                crate::runtime::jvmti::fire_method_exit_for_vm(
                    shared.vm_identity,
                    tid,
                    method_id,
                    true,
                    crate::runtime::jvmti::LocalValue::Object(None),
                )
            });
        }
    }
    // AN `ACC_SYNCHRONIZED` FRAME A FAST DOOR PUSHED RELEASES ITS MONITOR HERE.
    //
    // The general path scopes that release to an RAII guard around the nested
    // Rust invoke (`SynchronizedMethodGuard`). A door pushes the callee into
    // THIS loop and returns, so there is no Rust scope to hang it on -- the
    // frame owns it instead, in the `monitor_on_exit` slot the stackless
    // design added and never filled.
    //
    // This is the only place it can go, and that is checkable rather than
    // hopeful: every removal of an interpreter frame reaches this function --
    // the return opcodes, the exception unwind (`was_popped_by_exception`),
    // and the orphan sweeps `execute` and `resume_continuation` run after
    // `execute_frame_from_index` returns early through `return Err`.
    //
    // Before the JVMTI hooks below, because a `FramePop` callback can run
    // arbitrary Java -- including code that wants this very monitor.
    //
    // The address is read from the frame rather than remembered by the door: a
    // moving collection between the acquire and the return relocates the
    // object, and `memory/gc.rs` already remaps `monitor_on_exit` for exactly
    // this reason.
    // Block monitors the frame's own `monitorenter`s still hold go first
    // (newest acquisitions before the method monitor, which was taken on
    // entry). A return or a Java-throwable unwind has already enforced
    // JVMS §2.11.10 and emptied the record; what reaches here is a frame
    // removed another way — a VM-error unwind, an orphan sweep, JVMTI
    // `PopFrame` — and JVMTI's rule for those is that the frame's locks are
    // released (`interpreter::held_monitors`, wave 23 lane L7).
    if thread
        .frames
        .last()
        .is_some_and(|f| !f.held_monitors.is_empty())
    {
        let top = thread.frames.len() - 1;
        held_monitors::release_all_recorded(shared, thread, top);
    }
    if thread
        .frames
        .last()
        .is_some_and(|f| f.monitor_on_exit.is_some())
    {
        let tid = thread.thread_id;
        let monitor = thread
            .frames
            .last_mut()
            .and_then(|f| f.monitor_on_exit.take());
        if let Some(monitor) = monitor {
            // B8: the frame MUST be recycled, so the error cannot propagate,
            // but swallowing it silently loses the diagnosis of a corrupted
            // monitor state (e.g. user code that `monitorexit`ed past the
            // synchronized method's own count). This is the ONLY release: the
            // `take()` above leaves `monitor_on_exit` empty, which is why the
            // second release that used to sit further down, after the JVMTI
            // hook, could never run and was removed.
            if let Err(e) = crate::vm::vm_exec::monitor_exit_and_retract_jmx(shared, monitor, tid) {
                if let Some(f) = thread.frames.last() {
                    tracing::warn!(
                        class = %f.class_name(),
                        method = %f.method_name(),
                        descriptor = %f.method_descriptor(),
                        thread_id = ?tid,
                        error = ?e,
                        "implicit monitorexit on synchronized-method-frame-pop failed"
                    );
                }
            }
        }
    }
    // T17.Δ.5 — JVMTI FramePop before the frame vanishes.
    fire_jvmti_frame_pop_if_requested(shared.vm_identity, thread, was_popped_by_exception);
    // The dying frame is read THROUGH THE STACK, not moved out of it.
    //
    // This block used to open with `if let Some(f) = thread.frames.pop()`, and
    // `f` then travelled into `recycle_frame_with_shared` and again into
    // `take_pool_parts` — three moves of a ~300-byte `Frame` to arrive at four
    // `Vec` headers. `perf` on the interpreted-invoke probe put `memcpy` under
    // `Vec::pop<Frame>` here, inside a frame-lifecycle group worth ~24.7% of
    // the invoke arm (see the annotation-scan known-issues page). Nothing below
    // needs the frame anywhere but where it already is.
    let depth = thread.frames.len();
    if depth > 0 {
        // Root-snapshot cache correctness: the frame that becomes the top again
        // (the caller this return/unwind exposes) is about to RE-EXECUTE and may
        // reassign its locals. Bump its `exec_epoch` so the `(seq, exec_epoch)`
        // cache key invalidates its stale cached roots — `seq` alone is unchanged
        // (the caller was never popped) and would otherwise reuse roots that miss
        // a freshly-allocated local (the AME all-zero-receiver corruption). See
        // the `Frame::seq` / `exec_epoch` docs. Cheap: one add on the (cold)
        // return/unwind path. `wrapping_add` so a (practically impossible) u64
        // overflow can never alias a live cache key into a false match.
        //
        // Hoisted above the borrow of the dying frame: with that frame still on
        // the stack the caller is at `depth - 2`, and the bump needs `&mut`.
        if depth >= 2 {
            let caller = &mut thread.frames[depth - 2];
            caller.exec_epoch = caller.exec_epoch.wrapping_add(1);
        }
        let f = &thread.frames[depth - 1];
        // Harvest this activation's loop work towards the method's tier-up
        // counter. This is the ONLY point at which the count is complete and
        // still attributable: `Frame::backward_count` is reset on every reuse,
        // so a loop that runs a few hundred iterations per call — the shape of
        // `ConstantPool.<init>` and `ClassParser.readFields` in Tomcat's
        // annotation scan — is otherwise thrown away wholesale, over and over,
        // and never reaches the OSR back-edge threshold within any one frame.
        //
        // Harvesting HERE rather than on a back-edge stride is deliberate: a
        // stride can only ever credit loops longer than the stride, which is
        // exactly the set this gap does NOT contain. Every iteration counts,
        // however short the loop, at a cost of one hash + one relaxed atomic
        // per *invocation that actually looped*.
        if f.backward_count > 0 && loop_work_tierup_enabled() {
            credit_loop_work(shared, f);
            // The short-loop page's `i + b` predicate (lane mon11, wave 11;
            // `CRATONVM_JIT_TIERUP_I_PLUS_B`, opt-in). Inside the credit's own
            // gate, so the plain-return fast path above needs no new test.
            if crate::runtime::env_cache::tierup_i_plus_b() {
                credit_i_plus_b(shared, f);
            }
        }
        // OSRENTRY6-3 (JIT round 19 proposal wave 7, lane interp7;
        // `CRATONVM_JIT_OSR_EARLY_REQUEST`, opt-in): the same complete count,
        // summed per METHOD for the early window's compile request
        // (`osr_early_request_due`).
        if f.backward_count > 0 && osr_early_request_armed() {
            jit_bridge::note_osr_method_backedges(shared, frame_invoc_key(f), f.backward_count);
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[FRAME_POP] depth={} {}.{}{}",
                thread.frames.len(),
                f.class_name(),
                f.method_name(),
                f.method_descriptor()
            );
        }
        // (`monitor_on_exit` is always empty here: the release at the top of
        // this function took it before the JVMTI hook ran. A second release
        // used to sit here and could never execute.)
        crate::runtime::frame::note_retired_frame_shape(f);
        // Harvest the four pooled `Vec`s out of the frame where it lies and let
        // `truncate` drop the husk in place — no `Frame` is moved. (A frame
        // whose buffers are a slot-slab window is retired the same way; the
        // retire releases its window.) `CRATONVM_DBG_INVOKE_PHASES=1` times it
        // for every pop (`invoke_phases::P_SLOT_RET`); off, `t0` is 0.
        let t0 = invoke_phases::now();
        thread.recycle_top_frame_in_place(
            &shared.mem.operand_stack_pool,
            &shared.mem.tag_pool,
            shared.config.frame_slot_reuse,
        );
        invoke_phases::charge_slot_ret(t0);
    }
}

/// The invocation-counter key of `f`'s method. A cached frame's entry
/// memoises exactly this key (`CachedBytecodeMethod::invoc_key`, same class id
/// and triple), so only an owned frame hashes the names here. Shared by
/// [`credit_loop_work`] and OSRENTRY6-3's per-method back-edge sum.
#[inline]
pub(in crate::runtime::interpreter) fn frame_invoc_key(f: &Frame) -> u128 {
    match f.cached_method() {
        Some(cached) => cached.invoc_key(),
        None => cratonvm_jit_api::invoc_key_parts(
            f.class_id.as_u32(),
            f.method_name(),
            f.method_descriptor(),
        ),
    }
}

/// OSRENTRY6-3 (`CRATONVM_JIT_OSR_EARLY_REQUEST`, JIT round 19 proposal wave 7,
/// lane interp7; default OFF): armed only together with MEGAOSR19-3's early
/// window (`CRATONVM_JIT_OSR_EARLY_REENTRY`), whose entry is what makes an
/// early-requested body worth compiling. Two cached-slot loads.
#[inline]
pub(in crate::runtime::interpreter) fn osr_early_request_armed() -> bool {
    crate::runtime::env_cache::osr_early_request() && crate::runtime::env_cache::osr_early_reentry()
}

/// ACCEPT21-4 (t1 wave 8, lane ACCEPT; book
/// `docs/feature-designs/wire-tiered-manager.md`, page
/// `docs/feature-designs/jit-osr-entry-metadata.md`
/// "Narrower condition"): `CRATONVM_JIT_OSR_EARLY_REQUEST_CHARGES_NOTHING=1`
/// (perf, **opt-in**, meaningful only under [`osr_early_request_armed`]). An
/// early OSR request the tiered manager refuses (`OsrRequest::Refused`) no
/// longer retires the header for the activation nor counts it against the
/// method-wide budget (`charge_osr_rejection`), the suspected cause of the
/// `R21AcceptOsrEarly late-callee-act` loss. The retry-budget half of the
/// page's condition (ask only while the manager's budget is untouched) needs a
/// `tiered.rs` query and is not built. Read only on a refused early request.
pub(in crate::runtime::interpreter) fn osr_early_request_charges_nothing_enabled() -> bool {
    false
}

/// JIT r20 wave 11 (lane pAlloc; book `jit-r19-osrentry6-proposals.md`
/// section 3; `CRATONVM_JIT_OSR_WAIT_CREDITS_LOOP_WORK`, default ON, `=0`
/// restores the discard): the OSR offer's background arm resets
/// `backward_count` (`Frame::record_osr_background_pending`) so the next poll
/// waits a full stride, and with it threw away the back edges the activation
/// had interpreted so far -- the frame pop's loop-work credit
/// ([`credit_loop_work`]) and the `i + b` / early-request sums then saw only
/// the edges since the LAST wait, under-crediting a looping method by up to
/// one OSR threshold per wait. This hands those edges to the same three sinks
/// the pop feeds, under the same gates, just before the reset; the pop later
/// credits only the edges after it, so no edge is counted twice. One hashed
/// counter add per wait (at most once per `osr_threshold` back edges).
/// `CRATONVM_DBG_LOOP_WORK` shows the extra credits (`[loop-work]` lines with
/// `backedges` near the OSR threshold while the compile is queued).
pub(in crate::runtime::interpreter) fn credit_loop_work_before_osr_wait(shared: &SharedVm, f: &Frame) {
    if f.backward_count == 0
    {
        return;
    }
    if loop_work_tierup_enabled() {
        credit_loop_work(shared, f);
        if crate::runtime::env_cache::tierup_i_plus_b() {
            credit_i_plus_b(shared, f);
        }
    }
    if osr_early_request_armed() {
        jit_bridge::note_osr_method_backedges(shared, frame_invoc_key(f), f.backward_count);
    }
}

/// Credit `f`'s back edges so far (`Frame::backward_count`) to its method's
/// tier-up counter (`ProfileStore::add_loop_work`), and nominate the method
/// for a whole-method compile when that credit carries the counter across
/// the invocation threshold. Called from the frame pop
/// (`CRATONVM_JIT_LOOP_WORK_TIERUP`) and, since round 16 (lane coldpath), from
/// an OSR entry ([`osr_entry_tierup_armed`]). `f.backward_count` must be > 0.
pub(in crate::runtime::interpreter) fn credit_loop_work(shared: &SharedVm, f: &Frame) {
    let key = frame_invoc_key(f);
    let total = shared
        .jit
        .profile_store
        .add_loop_work(key, f.backward_count);
    // Crossing the threshold is not enough: the ONLY place that acts on
    // the counter is the dispatch site, and it tests
    // `cnt == threshold || (cnt - threshold) % 64 == 0` against the value
    // ITS OWN increment returned. Credit applied here lands between two
    // dispatches, so those exact trigger points are simply stepped over
    // — `ConstantPool.<init>` was measured reaching a count of 1149,
    // more than twice the threshold, without ever being nominated.
    // Nominate from here instead, the same way the dispatch site does.
    let threshold = shared.config.jit_invocation_threshold;
    if total >= threshold && crate::runtime::env_cache::bg_compile() {
        // Re-nominating an already-published method is cheap and
        // idempotent (the manager dedups), so a coarse retry stride is
        // enough to cover a nomination the worker dropped.
        let crossed_now = loop_work_credit_crossed(total, f.backward_count, threshold);
        if crossed_now
            || total % 64 == 0
            || loop_work_retry_boundary_crossed(total, f.backward_count)
        {
            ensure_bg_compiler_started(shared);
            let tiered_key = crate::jit::tiered::MethodKey::with_class_id(
                f.class_id,
                f.class_name(),
                f.method_name(),
                f.method_descriptor(),
            );
            let _ = shared
                .jit
                .tiered_manager
                .on_method_invocation_observed(&tiered_key, total as u64);
        }
    }
    // `CRATONVM_DBG=loop-work` — the lever's own witness. A tier-up
    // change that cannot be seen doing anything is indistinguishable
    // from an inert one, and this lever has already been inert twice.
    if loop_work_dbg() {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 40 || total >= crate::runtime::env_cache::jit_invocation_threshold() {
            eprintln!(
                "[loop-work] {}.{}{} backedges={} count_now={}",
                f.class_name(),
                f.method_name(),
                f.method_descriptor(),
                f.backward_count,
                total
            );
        }
    }
}

/// HotSpot's `Tier3MinInvocationThreshold`: the `i + b` predicate
/// ([`i_plus_b_due`]) never fires for a method called fewer times than this.
pub(crate) const I_PLUS_B_MIN_INVOCATIONS: u32 = 100;

/// HotSpot's `Tier3CompileThreshold`: the `i + b` bar ([`i_plus_b_due`]).
pub(crate) const I_PLUS_B_COMPILE_THRESHOLD: u32 = 2_000;

/// Raw summed back edges between two retry nominations of a method the
/// `i + b` predicate already admits ([`credit_i_plus_b`]): a nomination the
/// worker dropped, or one the manager refused, is asked again once per this
/// many back edges, not once per looping pop.
pub(in crate::runtime::interpreter) const I_PLUS_B_RETRY_BACKEDGES: u32 = 2_048;

/// HotSpot's Tier3 call predicate on its back-edge half
/// (`CompilationPolicy::call_predicate_helper`, `CompLevel_none`):
/// `i >= Tier3MinInvocationThreshold && i + b >= Tier3CompileThreshold`,
/// with `b` the method's RAW summed back edges. (Its other half,
/// `i >= Tier3InvocationThreshold`, is the ordinary invocation threshold.)
#[inline]
pub(crate) fn i_plus_b_due(invocations: u32, backedges: u32) -> bool {
    invocations >= I_PLUS_B_MIN_INVOCATIONS
        && invocations.saturating_add(backedges) >= I_PLUS_B_COMPILE_THRESHOLD
}

/// Should this pop's `i + b` verdict nominate the method? Only when the
/// predicate holds now and either did not hold at the method's previous pop
/// (`invocations_before`, `backedges_before`) or the raw sum crossed another
/// [`I_PLUS_B_RETRY_BACKEDGES`] boundary.
#[inline]
pub(crate) fn i_plus_b_nominates(
    invocations_before: u32,
    backedges_before: u32,
    invocations: u32,
    backedges: u32,
) -> bool {
    i_plus_b_due(invocations, backedges)
        && (!i_plus_b_due(invocations_before, backedges_before)
            || backedges_before / I_PLUS_B_RETRY_BACKEDGES != backedges / I_PLUS_B_RETRY_BACKEDGES)
}

/// The short-loop page's `i + b` predicate
/// (`r19-short-loop-methods-called-under-500-times-never-compile-OBSOLETE-20261012.md`;
/// lane mon11, JIT round 19 proposal wave 11; `CRATONVM_JIT_TIERUP_I_PLUS_B`,
/// opt-in, called only beside [`credit_loop_work`]).
///
/// The loop-work credit adds `backward_count` over `LOOP_WORK_PER_INVOCATION`
/// (32) per activation (`profile::loop_work_credit`), so an activation of a
/// loop shorter than 32
/// iterations credits NOTHING, however often it runs: a method called 300
/// times with a 20-iteration loop never compiles (300 < 500), while HotSpot
/// compiles it at its 100th call (`100 + 2000 >= 2000`). This sums the RAW
/// back edges per method (`jit_bridge::note_tierup_method_backedges`) and
/// nominates the whole-method compile on HotSpot's predicate, fast-forwarding
/// the manager's count to its C1 bar so its ordinary recommendation fires.
/// Additive: the credit's own threshold still nominates everything it did.
///
/// `i` is the method's profile-store count, which already holds the scaled
/// credit (a few percent of `b`); the bar is not tuned for that.
pub(in crate::runtime::interpreter) fn credit_i_plus_b(shared: &SharedVm, f: &Frame) {
    let key = frame_invoc_key(f);
    let Some(backedges) = jit_bridge::note_tierup_method_backedges(shared, key, f.backward_count)
    else {
        return;
    };
    let backedges_before = backedges.saturating_sub(f.backward_count);
    let invocations = shared.jit.profile_store.invocation_count(key);
    let threshold = shared.config.jit_invocation_threshold;
    // At or past the ordinary threshold the dispatch site and the loop-work
    // credit own the nomination.
    if invocations >= threshold || !crate::runtime::env_cache::bg_compile() {
        return;
    }
    // This pop's invocation and its credit, taken back off for "the previous
    // pop" (approximate under concurrent callers; the retry stride covers it).
    let invocations_before = invocations
        .saturating_sub(1)
        .saturating_sub(crate::jit::profile::loop_work_credit(f.backward_count));
    if !i_plus_b_nominates(invocations_before, backedges_before, invocations, backedges) {
        return;
    }
    ensure_bg_compiler_started(shared);
    let c1_bar = shared
        .jit
        .tiered_manager
        .policy()
        .c1_threshold
        .max(threshold);
    let tiered_key = crate::jit::tiered::MethodKey::with_class_id(
        f.class_id,
        f.class_name(),
        f.method_name(),
        f.method_descriptor(),
    );
    let recommended = shared
        .jit
        .tiered_manager
        .on_method_invocation_observed(&tiered_key, u64::from(c1_bar));
    if loop_work_dbg() {
        eprintln!(
            "[loop-work] i+b {}.{}{} i={invocations} b={backedges} -> nominated ({})",
            f.class_name(),
            f.method_name(),
            f.method_descriptor(),
            if recommended.is_some() {
                "recommended"
            } else {
                "no recommendation"
            }
        );
    }
}


// ---------------------------------------------------------------------------
// OSR back-edge orchestration helper
// ---------------------------------------------------------------------------

/// Outcome of a back-edge OSR attempt. Returned by [`try_osr_with_backoff`]
/// so callers can dispatch cleanly between the three possible outcomes
/// (return-from-execute_frame, continue-the-dispatch-loop, fall-through-to-
/// safepoint).
///
/// Round-7 HIGH #1: the surrounding OSR-rejection orchestration used to be
/// duplicated 14 times at each back-edge site (goto/if_icmp{eq,ne,lt,le,gt,
/// ge}/if{eq,ne,lt,le,gt,ge}/super-instruction iload-iload-if_icmplt). All
/// 14 callsites shared the identical sequence:
///
///   1. `should_try_osr(entry_pc, OSR_THRESHOLD)` — backoff schedule check.
///   2. `try_osr(...)` — JIT compile + entry trampoline.
///   3. On `Some(value)`: pop child frame if any, push return value, then
///      either `continue` the dispatch loop (nested call) or `return Ok(v)`
///      (root-frame return).
///   4. On `None`: `charge_osr_rejection` to schedule the next retry
///      exponentially further out (and, once the activation's five offers
///      are all refused, to charge the method-wide budget).
///
/// This helper folds 1–4 into a single call so each callsite shrinks to
/// the dispatch-match on `OsrBackoffOutcome` plus the natural
/// `safepoint_check`/`continue` tail.
pub(crate) enum OsrBackoffOutcome {
    /// OSR didn't fire (either backoff not yet, or `try_osr` rejected and
    /// the rejection has been recorded). Caller falls through to its
    /// post-back-edge work (typically `safepoint_check` then `continue`).
    ///
    /// Since the RBC.6b lift this also covers a case where OSR very much DID
    /// fire: the OSR'd body raised an exception this method catches, and the
    /// live frame has been left parked at the handler. The caller's action is
    /// identical — resume interpreting this frame — but no rejection is
    /// recorded, because nothing was rejected. See `try_osr`'s `committed_out`.
    Skip,
    /// OSR completed and we're back at the root frame of this
    /// `execute_frame` invocation — bubble the return value up to the
    /// outer caller via `return Ok(value)`.
    ReturnOuter(Option<Value>),
    /// OSR completed an inner (stackless) call frame; the helper has
    /// already popped the recycled frame and pushed the return value
    /// onto the now-current frame's stack. Caller should `continue` the
    /// dispatch loop so it picks up at the parent's next instruction.
    /// The new current frame index is communicated back via the
    /// `frame_idx` out-parameter (which the helper mutates).
    ContinueDispatch,
    /// The OSR'd code exited with a Java exception in flight that this frame
    /// cannot catch. Until the RBC.6b lift that was a property of the whole
    /// population — an OSR'd method provably declared no exception table — and
    /// now it is a per-throw verdict reached by
    /// `route_osr_exception_out_of_artifact`: either no handler covers the
    /// precise throw bci, or the throw site lies outside every protected range.
    /// The caller must hand the throwable to the dispatch loop's
    /// `pending_java_exception` channel so it unwinds from THIS frame, instead
    /// of resuming the loop.
    ///
    /// The old behaviour here was `Skip` + a re-stashed exception, i.e.
    /// "keep interpreting this frame from where it was". That is correct only
    /// when the bail precedes any committed loop iteration; the exception can
    /// surface at any invoke, arbitrarily far into the loop, and every
    /// iteration the OSR'd body had already committed was then executed a
    /// second time by the interpreter. See the retired
    /// `jit-osr-bail-on-callee-exception-reruns-loop-iterations` write-up.
    ThrowJava(ObjectRef),
}

/// `CRATONVM_JIT=loop-work-tierup` — count loop iterations towards the method
/// invocation threshold. Read once and cached; this sits on the interpreter's
/// back-edge path. Default ON since the JIT round 19 proposal round's close
/// (2026-10-09: `R19Handler8Rows` cold-never-ctl 437 -> 4 ns, the short-loop
/// methods called under 500 times now tier up; batteries equal); `=0` restores
/// the old count.
pub(in crate::runtime::interpreter) fn loop_work_tierup_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        true
    })
}

/// `CRATONVM_DBG=loop-work` — trace what [`loop_work_tierup_enabled`] actually
/// credits, so an inert lever is visibly inert instead of quietly so.
pub(in crate::runtime::interpreter) fn loop_work_dbg() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| None::<std::ffi::OsString>.is_some())
}

/// `CRATONVM_JIT_OSR_ENTRY_TIERUP` (default ON, `=0` off; JIT round 16, lane
/// coldpath): an activation whose loop was hot enough to ENTER an OSR body
/// credits the back edges it interpreted before the entry to the method's
/// tier-up counter, as the frame pop does for every activation under
/// `CRATONVM_JIT_LOOP_WORK_TIERUP` -- HotSpot's back-branch event, which also
/// asks whether the method has earned a standard compile (`i + b`).
///
/// Without it a method whose work is one hot loop per call (a fresh
/// `HashMap` filled per call, `HashMap.resize`) was entered through OSR on
/// every call -- interpreting `osr_threshold` back edges first each time --
/// until 500 plain invocations nominated its whole-method compile.
/// `R16ColdpathHashMap` P1/P2/P5 warm-up rounds ran 3-5x slower than the
/// steady state for 500 calls (r16-coldpath page). Only the entry pays: one
/// hashed counter add per OSR entry, nothing on any back edge or frame pop.
///
/// Off while `CRATONVM_JIT_LOOP_WORK_TIERUP` is on: that pop credits the same
/// back edges, at the same frame, and must not count them twice.
pub(in crate::runtime::interpreter) fn osr_entry_tierup_armed() -> bool {
    cratonvm_types::flags::flags().jit.osr_entry_tierup && !loop_work_tierup_enabled()
}

/// Did the loop-work credit for `iterations` back edges carry the method's
/// counter -- now `total`, as `ProfileStore::add_loop_work` returned it --
/// across `threshold`? Subtracts the credit the store applied
/// (`profile::loop_work_credit`, the store's own rounding) rather than a copy
/// of its divisor, which silently mis-fired whenever the jit crate retuned it
/// (`interpreter-L7-loop-work-credit-divisor-duplicates-a-jit-constant-FIXED`).
#[inline]
pub(in crate::runtime::interpreter) fn loop_work_credit_crossed(total: u32, iterations: u32, threshold: u32) -> bool {
    total >= threshold
        && total.saturating_sub(crate::jit::profile::loop_work_credit(iterations)) < threshold
}

/// JIT round 21 (lane osrreentry; `CRATONVM_JIT_LOOP_WORK_RETRY_CROSSING`,
/// perf, default ON, `=0` restores the exact-multiple test alone): did the
/// loop-work credit for `iterations` back edges carry the method's counter --
/// now `total` -- across a multiple of 64, the retry stride of
/// [`credit_loop_work`]'s re-nomination?
///
/// The stride used to be `total % 64 == 0`, which the credit steps over for
/// the reason that function's own comment gives for the threshold: it adds
/// `iterations / 32` at once (31 for an activation that reached the OSR
/// threshold), so a method whose activations are alike lands on the same two
/// residues mod 64 forever, and the dispatch site's own `+1` tests sit between
/// those jumps. A method whose body a trap evicted (`current_tier` back to the
/// interpreter) and whose recompile request was dropped was then re-nominated
/// by luck of its residue, possibly never. The flag is read only
/// when this answer differs from the old one (at most once per 64 credits).
#[inline]
pub(in crate::runtime::interpreter) fn loop_work_retry_boundary_crossed(total: u32, iterations: u32) -> bool {
    let credit = crate::jit::profile::loop_work_credit(iterations);
    credit > 0
        && total % 64 != 0
        && total / 64 != total.saturating_sub(credit) / 64
}

/// Run the standard back-edge OSR orchestration: backoff check → try_osr →
/// rejection bookkeeping. See [`OsrBackoffOutcome`] for the meaning of
/// each return variant.
///
/// **Borrow contract:** callers MUST drop any outstanding `&mut Frame`
/// borrow before calling this — the helper re-borrows `thread.frames`
/// internally (both for the should-try check and for the rejection
/// record). The standard sequence at each callsite is:
///
/// ```ignore
/// frame.backward_count += 1;
/// let entry_pc = frame.pc;
/// let _ = frame; // drop &mut borrow
/// match try_osr_with_backoff(shared, thread, &mut frame_idx,
///                             initial_frame_idx, entry_pc) {
///     OsrBackoffOutcome::ReturnOuter(v) => return Ok(v),
///     OsrBackoffOutcome::ContinueDispatch => continue,
///     OsrBackoffOutcome::ThrowJava(exc) => {
///         pending_java_exception = Some((exc, OSR_FRAME_DECLINED_TO_CATCH));
///         continue;
///     }
///     OsrBackoffOutcome::Skip => {}
/// }
/// safepoint_check(shared, thread);
/// ```
///
/// **Poll point (interpreter round i1 wave 16, lane L5).** Every `Skip`
/// leaves the frame's next poll in `Frame::osr_poll_at` ([`osr_next_poll`]),
/// which the back-edge sites compare before calling here again. A loop header
/// this activation retired (its five offers refused, or the method-wide budget
/// spent, or `--nojit`'s refusals) used to reach this function on EVERY back
/// edge past the floor for the rest of the activation, only for
/// `Frame::should_try_osr` to answer "no"; it now arrives once per
/// [`OSR_POLL_STRIDE`] back edges. A header that is due keeps being polled on
/// every back edge, exactly as before, and one waiting on its backoff is
/// polled at its own due point, so a frame with one loop sees its offers at
/// the same back edges as before. A second loop header of the same frame is
/// offered at most `OSR_POLL_STRIDE` back edges late. The arrival trace and
/// `CRATONVM_JIT_NO_OSR_INLINE_GATE=1` (floor 0) keep the call on every back
/// edge.
///
/// `#[inline(never)]` since interpreter round i1 wave 27 (lane L7; it was
/// `#[inline]`): the dispatch loop's twenty back-edge macro expansions each
/// call it only past the OSR floor and the frame's poll point (never under
/// `--nojit`), so inlining it put twenty copies of the poll-point bookkeeping
/// -- and, fat LTO permitting, of `try_osr_offer`, whose only caller this is
/// -- into the loop body that every bytecode runs in.
#[inline(never)]
pub(crate) fn try_osr_with_backoff(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: &mut usize,
    initial_frame_idx: usize,
    entry_pc: usize,
) -> OsrBackoffOutcome {
    let outcome = try_osr_offer(shared, thread, frame_idx, initial_frame_idx, entry_pc);
    // `Skip` leaves the same frame at `*frame_idx` (every other outcome popped
    // it, or unwinds it).
    if matches!(outcome, OsrBackoffOutcome::Skip) && osr_poll_point_armed() {
        let osr_threshold = crate::runtime::env_cache::tier_osr_backedge().unwrap_or(OSR_THRESHOLD);
        if let Some(frame) = thread.frames.get_mut(*frame_idx) {
            frame.osr_poll_at = osr_next_poll(frame, entry_pc, osr_threshold);
            // MEGAOSR19-3: below the threshold no header is due (every due
            // point is `osr_threshold << attempts`), so a frame polled early
            // (`osr_early_reentry_due`) waits for the threshold rather than
            // re-polling every stride.
            if frame.backward_count < osr_threshold
                && crate::runtime::env_cache::osr_early_reentry()
            {
                frame.osr_poll_at = frame.osr_poll_at.max(osr_threshold);
            }
        }
    }
    outcome
}

/// Back edges between two out-of-line OSR polls of a frame whose current loop
/// header is not due (HotSpot's `Tier0BackedgeNotifyFreqLog` plays this part
/// there). Also the most a second loop header of the same frame can be offered
/// late. See [`try_osr_with_backoff`].
pub(crate) const OSR_POLL_STRIDE: u32 = 256;

/// Kill switch for the per-frame poll point, a `const` so the unarmed build
/// pays nothing: `false` restores the call on every back edge past the floor.
pub(in crate::runtime::interpreter) const OSR_POLL_POINT_ENABLED: bool = true;

/// Is the poll point in force? Not while something needs every arrival: the
/// arrival trace and `CRATONVM_JIT_NO_OSR_INLINE_GATE=1`, which already hold
/// the floor at 0 for that reason.
#[inline]
pub(crate) fn osr_poll_point_armed() -> bool {
    OSR_POLL_POINT_ENABLED
        && !crate::runtime::env_cache::no_osr_inline_gate()
        && !osr_frame_trace::enabled()
}

/// The `backward_count` at which `frame`'s next OSR poll is due, after a
/// declined offer at `entry_pc`: the header's own next due point
/// (`Frame::should_try_osr`'s `osr_threshold << attempts`; never, once the
/// header is retired) or [`OSR_POLL_STRIDE`] back edges from now, whichever
/// is sooner. A due point at or below the current count keeps the poll on
/// every back edge. Near `u32::MAX` it answers `0`, polling every back edge
/// until the count wraps, so a frame can never be left with a poll point it
/// will not reach again.
#[inline]
pub(crate) fn osr_next_poll(frame: &Frame, entry_pc: usize, osr_threshold: u32) -> u32 {
    let bc = frame.backward_count;
    let stride_point = bc.checked_add(OSR_POLL_STRIDE).unwrap_or(0);
    let attempts = frame.osr_attempts_at(entry_pc).unwrap_or(0);
    let due = if attempts >= crate::runtime::frame::OSR_MAX_ATTEMPTS {
        u32::MAX
    } else {
        // The same expression `should_try_osr` compares against.
        osr_threshold.checked_shl(attempts).unwrap_or(u32::MAX)
    };
    due.min(stride_point)
}

/// The body of [`try_osr_with_backoff`], which records the frame's next poll
/// point around it.
pub(in crate::runtime::interpreter) fn try_osr_offer(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: &mut usize,
    initial_frame_idx: usize,
    entry_pc: usize,
) -> OsrBackoffOutcome {
    // osr-02 frame comparator: record the back-edge ARRIVAL here, ahead of
    // every early return below.
    //
    // The placement is the whole point. This function is the one funnel all
    // fourteen back-edge sites go through, and the records have to be taken
    // under conditions that do NOT depend on OSR — the ground-truth arm runs
    // `--nojit`, where every check below would decline. A hook placed after the
    // virtual-thread test, the `CRATONVM_JIT_OSR` gate or the backoff schedule
    // would emit in one arm and not the other, and the comparison would be
    // between two different things rather than between two runs of one.
    //
    // Costs one `OnceLock` bool load when the flag is unset, on a path that
    // already performs several cached environment reads.
    if osr_frame_trace::enabled() {
        osr_frame_trace::record_arrival(&thread.frames[*frame_idx], entry_pc);
    }
    // `--nojit` (`CRATONVM_DISABLE_JIT`): nothing can be entered, and the
    // method-entry door already declines on the same flag. Offering anyway
    // queued a background OSR compile per loop header (starting the compiler
    // threads under `--nojit`) for the worker to refuse with `JitDisabled`
    // (interpreter round i1 wave 16, lane L5).
    // A virtual thread is refused, except a JDK one under
    // `CRATONVM_VTHREAD_COMPILED_LEAF_OSR` (interpreter round i5 wave 100,
    // lane L6), whose frame's method is judged below, once an offer is due.
    // Wave 101 (lane L2): or `CRATONVM_VTHREAD_COMPILED_LEARNED`
    // (`jdk_continuation::vthread_osr_door_on`).
    let vthread_osr_judged = matches!(thread.kind, crate::threading::ThreadKind::Virtual);
    if (vthread_osr_judged && !crate::vm::jdk_continuation::vthread_osr_door_on(shared, thread))
        || crate::runtime::env_cache::disable_jit()
    {
        return OsrBackoffOutcome::Skip;
    }
    // Back-edge OSR is default-on after the known entry-state corruption
    // blockers were retired. Whole-method JIT is unaffected. `CRATONVM_JIT_OSR=0`
    // opts out for diagnosis/bisection. This is the canonical entry for BOTH the
    // inline and background-OSR paths, so the gate disables OSR everywhere.
    if !crate::runtime::env_cache::osr_backedge_enabled() {
        return OsrBackoffOutcome::Skip;
    }
    // wire-tiered-manager Step 6: the per-frame back-edge OSR trigger is now an
    // env knob (`CRATONVM_TIER_OSR_BACKEDGE`); `OSR_THRESHOLD` remains the
    // canonical default. Live on both the inline and background-OSR paths. Unset
    // → identical to before.
    let osr_backedge_threshold =
        crate::runtime::env_cache::tier_osr_backedge().unwrap_or(OSR_THRESHOLD);
    // Loop work performed across SHORT invocations is otherwise invisible to
    // tier-up, and that gap is what keeps Tomcat's BCEL annotation scan
    // interpreted. Both counters miss it:
    //
    //   * the method invocation counter accumulates globally, but
    //     `ConstantPool.<init>` / `ClassParser.readFields` run ONCE PER CLASS —
    //     468 calls over the whole probe, under the 500 threshold;
    //   * `Frame::backward_count` is per-frame and reset on every invocation
    //     (`Frame::reset`), so a ~300-iteration constant-pool loop never reaches
    //     the 1000-back-edge OSR threshold WITHIN one frame — no matter how many
    //     classes are parsed. That is permanent, not a warm-up artifact.
    //
    // Measured: with the stock thresholds this workload reports `osr=0` — not a
    // single OSR body in the entire scan — while the per-constant methods it
    // calls (`Constant.readConstant`, `ConstantUtf8.getInstance`) compile fine.
    // See docs/feature-designs/real-jdk-mode.md.
    //
    // Credit loop work towards that same invocation counter, the way HotSpot
    // sums its invocation and back-edge counters against a single threshold.
    // This reuses the existing counter and compile path exactly — no new state,
    // and no OSR involvement: the method simply crosses the ordinary threshold
    // and its NEXT call runs compiled.
    //
    // The credit itself is applied in `pop_and_recycle_frame_with_reason`, at
    // the one point where an activation's back-edge count is both complete and
    // still attributable. See `ProfileStore::add_loop_work`.
    // MEGAOSR19-3 (`CRATONVM_JIT_OSR_EARLY_REENTRY`, opt-in): before the
    // schedule's first due point, an activation whose header already has an
    // enterable body cached is offered anyway (`osr_early_reentry_due`). An
    // early offer that does not enter charges nothing to the header's budget,
    // so the ordinary schedule below is unchanged by it.
    let scheduled = thread.frames[*frame_idx].should_try_osr(entry_pc, osr_backedge_threshold);
    let early_entry = !scheduled
        && osr_early_reentry_due(shared, thread, *frame_idx, entry_pc, osr_backedge_threshold);
    // OSRENTRY6-3 (`CRATONVM_JIT_OSR_EARLY_REQUEST`, opt-in): the early window
    // with NO body cached may REQUEST the compile once the method's summed
    // back edges crossed the threshold. It then takes the background arm's
    // ordinary request (and its refusal bookkeeping) below.
    let early_request = !scheduled
        && !early_entry
        && osr_early_request_due(shared, thread, *frame_idx, entry_pc, osr_backedge_threshold);
    if !scheduled && !early_entry && !early_request {
        return OsrBackoffOutcome::Skip;
    }
    // Wave 100 (lane L6, `CRATONVM_VTHREAD_COMPILED_LEAF_OSR`): a JDK virtual
    // thread's frame enters OSR code only when its whole method cannot reach
    // a yield (a compiled activation cannot be frozen; a yield under one
    // would pin). An `Owned` frame (no shared method record) is refused. Only
    // past the schedule, so the scan runs once per due offer, not per poll.
    if vthread_osr_judged {
        let Some(method) = thread.frames[*frame_idx].cached_method().cloned() else {
            return OsrBackoffOutcome::Skip;
        };
        if !crate::vm::jdk_continuation::vthread_may_osr_method(shared, thread, &method) {
            // Interpreter round i5 wave 107, lane L6 (page `i91-L6`, "Wave
            // 107"): under `CRATONVM_VTHREAD_COMPILED_LEARNED` alone the
            // refusal cannot change within this activation (the thread kind,
            // `synchronized`, background compilation and the switch are
            // fixed, and the deny set only grows), so retire the header for
            // the activation. Without this the refusal recorded nothing:
            // `osr_next_poll` found the header still due and kept the poll on
            // every back edge past the floor -- an out-of-line call, an `Arc`
            // clone and a deny-set probe per iteration of a denied method's
            // loop (`helpersTask`'s last ~200 of 1 208 back edges per
            // activation in `L6W103VirtualThreadLearnedShortBench`). Set
            // directly, not through `charge_osr_rejection`: the method-wide
            // record is shared with platform threads, which this refusal
            // says nothing about. `CRATONVM_VTHREAD_COMPILED_LEAF_OSR`'s
            // answer can change (a leaf caller's sites fill as it runs), so
            // it keeps the old poll.
            if !shared.config.vthread_compiled_leaf_osr {
                thread.frames[*frame_idx]
                    .set_osr_attempts(entry_pc, crate::runtime::frame::OSR_MAX_ATTEMPTS);
            }
            return OsrBackoffOutcome::Skip;
        }
    }
    // A JVMTI agent listening for events compiled code never posts: an OSR'd
    // loop body would run its remaining iterations — every bytecode, every
    // call, every field access in them — without a single SingleStep,
    // MethodEntry/Exit, FramePop or field-watch event. HotSpot answers the
    // same question with `interp_only_mode`. Checked only once the schedule
    // above says "attempt", so the no-agent back edge pays nothing; nothing is
    // recorded, so OSR resumes on its normal schedule once the agent stops
    // listening. The whole-method doors ask the same per-VM question
    // (`jvmti_requires_interpreter`). The JDWP half asks about THIS method
    // only (wave 8, lane L1): one breakpoint used to stop OSR everywhere.
    let osr_frame = &thread.frames[*frame_idx];
    if jvmti_requires_interpreter_for_method(
        shared,
        osr_frame.class_id,
        osr_frame.method_name(),
        osr_frame.method_descriptor(),
    ) {
        return OsrBackoffOutcome::Skip;
    }
    // A frame running a body its class's redefinition replaced: OSR bodies
    // are compiled from the class's CURRENT bytecode and found by method
    // name, so entering one at this frame's pc would run other code with
    // this frame's locals (interpreter round i1 wave 19, lane L3). One load
    // for a frame built after the process's last redefinition.
    if obsolete_frames::frame_runs_replaced_code(shared, osr_frame) {
        return OsrBackoffOutcome::Skip;
    }
    // JIT round 21 wave 13 (review rev-jit, item 1 of
    // `docs/feature-designs/jit-ir-optimizations.md`): nor a
    // finalizable class's constructor that can throw before its `super(..)`
    // (`exception_dispatch::finalizer_bars_compiling`, asked by every other
    // compile door). An OSR body's throw reaches the unwinder at
    // `OSR_FRAME_DECLINED_TO_CATCH`, which the proof cannot place, so a
    // constructor whose pre-`super` loop OSR'd kept a registration HotSpot
    // never makes. The answer is fixed for the method, so the header is
    // retired for the activation (as the virtual-thread refusal above does):
    // otherwise the unrecorded refusal would be re-asked on every back edge.
    let osr_ctor_barred = finalizer_bars_compiling(
        shared,
        osr_frame.class_id,
        osr_frame.method_name(),
        osr_frame.method_descriptor(),
    );
    if osr_ctor_barred {
        thread.frames[*frame_idx]
            .set_osr_attempts(entry_pc, crate::runtime::frame::OSR_MAX_ATTEMPTS);
        return OsrBackoffOutcome::Skip;
    }
    // The method-wide budget, asked once per activation and loop header: a
    // loop whose offers were ALL refused in `OSR_REFUSED_ACTIVATIONS_BUDGET`
    // earlier activations is not offered again at this install epoch (bar a
    // periodic re-probe). The frame's own five-offer budget is reset by every
    // activation, so without this a refused loop cost five offers -- five
    // compile pipelines when compiling inline -- per call, forever
    // (`interpreter-L7-osr-refusals-reoffered-per-activation-FIXED`, fix 3). One
    // load when no
    // loop in the VM has spent a whole activation's offers.
    if thread.frames[*frame_idx]
        .osr_attempts_at(entry_pc)
        .is_none()
    {
        let allowed =
            jit_bridge::osr_loop_offer_allowed(shared, &thread.frames[*frame_idx], entry_pc);
        thread.frames[*frame_idx].set_osr_attempts(
            entry_pc,
            if allowed {
                0
            } else {
                crate::runtime::frame::OSR_MAX_ATTEMPTS
            },
        );
        if !allowed {
            return OsrBackoffOutcome::Skip;
        }
    }
    // j27 wave 5 (lane OSRRETIRE, `CRATONVM_JIT_OSR_LIVE_STACK_RETIRE`): a
    // header whose operand stack is live is refused by every OSR door, so it
    // is retired here, before either compile arm (`osr_live_stack`).
    if osr_live_stack::retire_live_stack_osr_header(shared, thread, *frame_idx, entry_pc) {
        return OsrBackoffOutcome::Skip;
    }
    // wire-tiered-manager Step 5 (precise background OSR): when the bg/tiered
    // pipeline is enabled, OSR compilation runs OFF the mutator. On a hot
    // back-edge (the per-frame `should_try_osr` schedule above is the throttle)
    // we enqueue an OSR task for the worker and KEEP INTERPRETING; we only enter
    // a worker-PUBLISHED `compiled_via_osr` artifact (the existing reuse path) —
    // never inline-compile here. `CRATONVM_BG_COMPILE` is default-ON
    // (`env_cache::bg_compile`); `=0` restores the historical inline OSR below.
    if crate::runtime::env_cache::bg_compile() {
        // The frame's own `Arc<str>` names: three refcount bumps, where
        // `to_string()` + the key's `Into<Arc<str>>` cost six allocations on
        // every offer — five per activation of a method whose loop is denied.
        let (cn, mn, md, frame_class_id) = {
            let f = &thread.frames[*frame_idx];
            (
                f.class_name_arc(),
                f.method_name_arc(),
                f.method_descriptor_arc(),
                f.class_id,
            )
        };
        // Three-way classification, not two. `reusable` is the enter-now case;
        // `published_but_unenterable` is a PUBLISHED OSR artifact that cannot be
        // entered at THIS pc, which is a permanent property, not a
        // still-compiling one (see the rejection arm below).
        let (reusable, published_but_unenterable) = {
            let jc = shared.jit.jit_cache.read();
            match jc.get_osr(&cn, &mn, &md, frame_class_id) {
                Some(c) if c.compiled_via_osr => {
                    let ok = c.can_osr_enter(entry_pc);
                    (ok, !ok)
                }
                _ => (false, false),
            }
        };
        // Tier proposal 26 (round 11 wave 9): an accepted OPTIMIZING OSR body
        // the worker cached for this pc is enterable too. Without this the
        // mutator reached `try_osr` — where that body is entered from — only
        // through a published single-pass body, so a loop whose single-pass
        // OSR compile was refused, or published without an entry here, stayed
        // interpreted beside an optimizing body it could have run
        // (r11w8-tier-optimizing-osr-route-unreachable-behind-single-pass).
        // `false` for a denied method and under either kill switch, so the arm
        // below is unchanged in both. Asked only when the single-pass body is
        // not reusable, i.e. once per `should_try_osr` stride while waiting.
        let reusable = reusable || {
            let f = &thread.frames[*frame_idx];
            jit_bridge::osr_optimizing_body_ready(
                shared,
                frame_class_id,
                &cn,
                &mn,
                &md,
                &f.code,
                entry_pc,
            )
        };
        // Which way each background offer went (`CRATONVM_DBG_JITC`): a loop
        // whose optimizing body was withdrawn must reach a request below, or
        // it stays interpreted for the rest of its activation.
        if crate::runtime::env_cache::dbg_jitc() {
            eprintln!(
                "[cratonvm-jitc] osr bg offer {cn}.{mn} pc={entry_pc} attempts={:?} reusable={reusable} unenterable={published_but_unenterable} early={early_entry}",
                thread.frames[*frame_idx].osr_attempts_at(entry_pc),
            );
        }
        if !reusable {
            // The early offer asked for a cached body only; one that vanished
            // since (a redefinition, a retirement) waits for the ordinary
            // schedule, which requests the compile.
            if early_entry {
                return OsrBackoffOutcome::Skip;
            }
            // Not compiled yet: ensure the worker is running, request an OSR
            // compile (idempotent), and back off so we re-probe later rather
            // than spin. A subsequent hot back-edge finds the published
            // artifact and falls through to the reuse-enter below.
            // Loader-aware: OSR denials and the in-flight slot are per class.
            let key = crate::jit::tiered::MethodKey::with_class_id(frame_class_id, cn, mn, md);
            // An artifact this same path already published, which reports
            // `can_osr_enter(entry_pc) == false`, will report that forever:
            // `osr_pc_to_native[entry_pc]` is a pure function of the bytecode
            // and the entry pc (the codegen writes `-1` for a pc strictly
            // inside a LICM-hoisted loop body, whose pre-header an OSR entry
            // would skip). Recompiling reproduces it byte for byte — the OSR
            // path passes empty branch/unroll hint maps, so nothing profile-
            // dependent can change the outcome; a class redefinition, the one
            // thing that would, replaces the cached artifact and re-evaluates
            // this check naturally.
            //
            // Treating it as "background compile pending" instead —
            // `record_osr_background_pending()` resets `backward_count` without
            // consuming the bounded per-pc rejection budget — made the loop
            // re-request a compile every `osr_threshold` back-edges forever.
            // Measured on the `CallRate.allocPutOld` probe: 200 full C2
            // pipelines for 200 000 iterations (one per 1 000), 199 of them
            // producing the identical un-enterable artifact, with the loop
            // interpreted throughout. Route it to the existing bounded
            // exponential-backoff schedule instead, which is already keyed
            // per-pc, so other loop headers in the same method keep their OSR
            // eligibility (unlike `mark_osr_denied`, which is method-wide).
            //
            // Strictly a waste-elimination change: it removes compiles, never
            // adds compiled execution. The loop runs interpreted either way.
            if shared.jit.tiered_manager.is_osr_denied(&key) || published_but_unenterable {
                // Counted, because this path is why `osr_entered=0` can appear
                // next to `osr_refused_entry=0` and a non-zero `osr=` compile
                // count — a combination that reads like "OSR was never even
                // tried" when in fact an artifact was built and found
                // un-enterable at this pc. `osr_refused_entry` is only recorded
                // inside `try_osr`, which this arm returns before reaching, so
                // without these two the whole OSR lifecycle line is silent about
                // the most common way OSR fails to happen.
                cratonvm_jit::metrics::record_osr_event(if published_but_unenterable {
                    "osr_published_but_unenterable"
                } else {
                    "osr_method_denied"
                });
                // Both are verdicts the activation's remaining four offers would
                // only read again (the denial stands for the install epoch, the
                // un-enterable pc for the bytecode), so the loop header is
                // retired for this activation at once and the activation counts
                // as fully refused against the method-wide budget.
                charge_osr_rejection(
                    shared,
                    thread,
                    *frame_idx,
                    entry_pc,
                    crate::runtime::frame::OSR_MAX_ATTEMPTS,
                );
                return OsrBackoffOutcome::Skip;
            }
            ensure_bg_compiler_started(shared);
            // nio20 wave 6, lane L3: the loop's `ldc <Class>` / `ldc <String>`
            // slots, minted on this mutator BEFORE the worker can start the OSR
            // compile (it may only find a slot, never create one), so the body
            // reads the constant instead of calling the `ldc` helper on every
            // iteration (`jit_bridge::mint_ldc_slots_at_offer`).
            {
                let code = std::sync::Arc::clone(&thread.frames[*frame_idx].code);
                let _ = jit_bridge::mint_ldc_slots_at_offer(shared, frame_class_id, &code);
                let _ = crate::jit::helpers::class_mirror_slot_for(shared, frame_class_id);
                jit_bridge::mint_const_ref_folds_at_offer(shared, Some(thread), frame_class_id, &code);
            }
            let request = shared
                .jit
                .tiered_manager
                .request_osr_classified(&key, entry_pc as u32);
            // CONF21-1: HotSpot resolves String constants when a compile is
            // requested; the queued OSR compile's resolve now, before the loop
            // runs on.
            jit_bridge::resolve_admitted_compile_strings(shared);
            if crate::runtime::env_cache::dbg_jitc() {
                eprintln!(
                    "[cratonvm-jitc] osr bg request {}.{} pc={entry_pc}: {}",
                    key.class_name,
                    key.method_name,
                    match &request {
                        crate::jit::tiered::OsrRequest::Queued(_) => "queued",
                        crate::jit::tiered::OsrRequest::Pending => "pending",
                        crate::jit::tiered::OsrRequest::Refused => "refused",
                    }
                );
            }
            if matches!(request, crate::jit::tiered::OsrRequest::Refused) {
                // The manager has given up on this method's OSR (`c2_bailout`,
                // `ineligible`, a spent retry budget -- none of which is an
                // OSR denial -- or tiered compilation off) and will refuse every
                // re-offer until a redefinition resets it. Retire the header like
                // the denial arm above, so the method-wide budget stops the
                // re-offers instead of paying `request_osr`'s VM-wide `methods`
                // mutex every `osr_threshold` back edges for the life of the
                // process
                // (`interpreter-L7-bg-osr-offers-continue-after-the-worker-gives-up-FIXED`).
                // Counted with the denials: from the loop's side it is one.
                cratonvm_jit::metrics::record_osr_event("osr_method_denied");
                // ACCEPT21-4 (t1 w8 lane ACCEPT, opt-in): an EARLY request the
                // manager refuses charges nothing, as an early entry already
                // does, so it is never the offer that retires the header or
                // spends the method-wide budget; the scheduled offer at the
                // threshold asks again and is charged as before.
                if early_request && osr_early_request_charges_nothing_enabled() {
                    if crate::runtime::env_cache::dbg_jitc() {
                        let f = &thread.frames[*frame_idx];
                        eprintln!(
                            "[cratonvm-jitc] osr early request refused (uncharged, ACCEPT21-4): {}.{}{} pc={entry_pc}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                        );
                    }
                    return OsrBackoffOutcome::Skip;
                }
                charge_osr_rejection(
                    shared,
                    thread,
                    *frame_idx,
                    entry_pc,
                    crate::runtime::frame::OSR_MAX_ATTEMPTS,
                );
                return OsrBackoffOutcome::Skip;
            }
            // Waiting for an off-thread compile is not a failed OSR attempt.
            // The old attempt counter reached its permanent cap within a few
            // thousand interpreted iterations, usually before the worker had
            // published, so the frame never probed the completed artifact.
            // Restart the stride instead: this polls at most once per threshold
            // while the task is queued. A permanent compile verdict marks the
            // method OSR-denied and a transient one spends the manager's retry
            // budget; either way a later offer lands in one of the two
            // retiring arms above.
            if early_request {
                cratonvm_jit::metrics::record_osr_event("osr_early_request");
            }
            // JIT r20 wave 11 (lane pAlloc, OSRENTRY6 section 3): the reset
            // below zeroes `backward_count`; credit what it would discard.
            credit_loop_work_before_osr_wait(shared, &thread.frames[*frame_idx]);
            thread.frames[*frame_idx].record_osr_background_pending();
            return OsrBackoffOutcome::Skip;
        }
        // `reusable`: fall through to `try_osr`, which reuses the published
        // artifact via its `osr_reused` fast path (no inline compile).
    }
    let osr_class_id = thread.frames[*frame_idx].class_id;
    // Out-channel for an exception the OSR'd body exited with (see
    // `OsrBackoffOutcome::ThrowJava`). `try_osr`'s return type has no error
    // arm, and widening it would touch every `return None` in a ~500-line
    // function; a single out-parameter written on exactly one path is the
    // minimal honest channel.
    let mut osr_throw: Option<ObjectRef> = None;
    // Sibling out-channel: the OSR'd body ran and advanced this frame, but
    // returned no value and threw nothing out — the RBC.6b lift's handler
    // entry. See `try_osr`'s parameter doc.
    let mut osr_committed = false;
    let depth_before_osr = thread.frames.len();
    let osr_result = try_osr(
        shared,
        thread,
        *frame_idx,
        osr_class_id,
        entry_pc,
        &mut osr_throw,
        &mut osr_committed,
    );
    // An entry that ran (threw out, committed, or returned) forgives the loop
    // header's earlier refused activations: the method-wide budget is for a
    // loop whose offers are ALWAYS refused (round 11 wave 15, lane tier; see
    // `jit_bridge::note_osr_loop_entered`). Asked while the frame is still at
    // `*frame_idx`, before the return arm below pops it.
    let entered_chain = osr_result.is_none() && thread.frames.len() > depth_before_osr;
    if early_entry
        && (osr_throw.is_some() || osr_committed || osr_result.is_some() || entered_chain)
    {
        cratonvm_jit::metrics::record_osr_event("osr_early_reentry");
    }
    // JIT round 21 (lane osrreentry; `CRATONVM_JIT_OSR_EARLY_REENTRY_CREDIT`,
    // default ON, `=0` off): an EARLY entry that ends the activation credits
    // the method's tier-up counter as the schedule's own entry would have.
    // Every sink of that counter (the pop's loop-work credit, the entry
    // credit below, the `i + b` and early-request sums) reads the frame's
    // INTERPRETED back edges, and the compiled body counts none. The
    // schedule's entry comes after `osr_backedge_threshold` interpreted back
    // edges (31 invocations' worth of credit per activation); the early one
    // after `OSR_EARLY_REENTRY_BACKEDGES` (1), so a method whose activations
    // all early-enter needed ~31x the calls to cross the invocation threshold,
    // and its method-entry body -- or, after a trap evicted that body, its
    // recompile -- never came: every call kept paying the OSR entry and ran
    // the OSR body (`jr21-osr-early-reentry-request-rows-FIXED-20261026.md`,
    // `late-callee-act` 45 -> 177 us with `CRATONVM_JIT_OSR_EARLY_REENTRY=1`).
    // A cached body was asked for by an activation that interpreted the
    // whole threshold, so the threshold is the count the schedule would have
    // seen. (Under `CRATONVM_JIT_OSR_EARLY_REQUEST` it may have been asked by
    // summed short activations instead; the credit then exceeds this
    // activation's loop and only brings the method-entry compile earlier.)
    // Not for a committed transfer or an entered chain: the frame goes on
    // interpreting and its count still drives the schedule.
    if early_entry
        && (osr_throw.is_some() || osr_result.is_some())
        && osr_early_reentry_credit_enabled()
    {
        let f = &mut thread.frames[*frame_idx];
        f.backward_count = f.backward_count.max(osr_backedge_threshold);
    }
    if osr_throw.is_some() || osr_committed || osr_result.is_some() {
        jit_bridge::note_osr_loop_entered(shared, &thread.frames[*frame_idx], entry_pc);
        // The entry that ENDS the activation (a return, or a throw out of it)
        // credits the interpreted back edges once (`osr_entry_tierup_armed`).
        // A committed transfer leaves the frame interpreting and may enter
        // again, re-reading the same `backward_count`, so it does not.
        if (osr_throw.is_some() || osr_result.is_some()) && osr_entry_tierup_armed() {
            let f = &thread.frames[*frame_idx];
            if f.backward_count > 0 {
                credit_loop_work(shared, f);
            }
        }
    }
    // The compiled body ran this activation's monitors: it may have released
    // a block monitor the frame's record names, and acquired others the
    // record does not. From here the record would only be wrong in the
    // dangerous direction (an entry for a lock no longer held, i.e. a false
    // `IllegalMonitorStateException` at the return), so it is emptied; a
    // later `monitorexit` of a lock the body left held is the unrecorded
    // acquisition `held_monitors` releases as before (JVMS §2.11.10, wave 23
    // lane L7).
    //
    // Not for a COMMITTED entry (interpreter round i1 wave 25, lane L2): every
    // path that commits — an OSR-exit transfer, a handler entry — rewrote the
    // record itself from the exit frame's description of the locks it holds
    // (`CompiledLocksOfAStash::replace_live_record_and_unpin`), or emptied it
    // where that description is not trusted (a single-pass body's). Emptying
    // it here, after the transfer, wiped that seed.
    if osr_throw.is_some() || osr_result.is_some() {
        // Interpreter round i6 wave 119, lane L6 (page
        // `i116-L9-strict-monitorexit-throws-on-an-understated-record`): a
        // throw is routed in THIS frame next, whose handler (javac's
        // catch-any of a `synchronized` block) runs `monitorexit` on the
        // emptied record: marked understated where the method can hold a
        // block monitor, so the strict arm stays lenient for it. A return
        // pops the frame, whose reuse clears the mark.
        held_monitors::clear_untrusted_record(
            &mut thread.frames[*frame_idx],
            held_monitors::RecordProducer::OsrBodyLeft,
        );
    }
    // Checked BEFORE the rejection bookkeeping below: the OSR'd body RAN (and
    // committed loop iterations), so this is not a rejected attempt and must
    // not consume the per-pc rejection budget.
    if let Some(exc) = osr_throw {
        return OsrBackoffOutcome::ThrowJava(exc);
    }
    // Round 13 wave 8 (lane chain3): an OSR exit whose frame is an inlined
    // chain (`deopt_resume::transfer_osr_exit_chain_into_live_frame`, planned
    // or planless) wrote the OUTERMOST scope into this frame, parked after its
    // `invoke`, and PUSHED the spliced callees above it. The dispatch loop
    // must continue in the top one: a `Skip` would run this frame on from the
    // invoke's successor without the callee's return value, with the callee
    // frames stranded above it. The bookkeeping is the frame's, as below.
    if entered_chain {
        if !osr_committed && !early_entry {
            charge_osr_rejection(shared, thread, *frame_idx, entry_pc, 1);
        }
        *frame_idx = thread.frames.len() - 1;
        return OsrBackoffOutcome::ContinueDispatch;
    }
    // Same rule, same reason, for the path that ran and CAUGHT. `Skip`'s "fall
    // through to your post-back-edge work" is the right action here — the frame
    // is parked at a handler with the throwable on its stack, so the dispatch
    // loop resumes there (after its safepoint check) and re-enters the cached
    // artifact at the next hot back-edge. What must NOT happen is the rejection
    // bookkeeping below: charging a caught exception against the per-pc budget
    // retires OSR after five of them.
    if osr_committed {
        return OsrBackoffOutcome::Skip;
    }
    match osr_result {
        Some(osr_val) => {
            if *frame_idx > initial_frame_idx {
                // Interpreter round i6 wave 116, lane L9 (review; page
                // `i116-L9-osr-return-skips-continuation-return-adapters`): a
                // remounted leaf frame that OSR'd and returned from its
                // compiled body still owes its door's return conversion, as
                // the interpreter's `return` arms pay it -- the `null` of a
                // `void` leaf behind a reference door
                // (`CRATONVM_MH_VOID_TAIL_UNMOUNT`) or a boxed / dropped value
                // (the lambda door, wave 25). Without it the parent resumed
                // one slot short (or with an unconverted slot). One `kind`
                // compare on any other return.
                if matches!(thread.kind, crate::threading::ThreadKind::Virtual)
                    && !thread.continuation_return_adapters.is_empty()
                {
                    // `Option<Value>` is `Copy`: `osr_val` stays for the
                    // plain return below.
                    // Interpreter round i6 wave 117, lane L2: the census line
                    // that proves a run reaches this arm
                    // (`CRATONVM_DBG_OSR_RETURN_ADAPTER=1`; page
                    // `i116-L9-osr-return-skips-continuation-return-adapters`).
                    // Cold: only a virtual thread with a pending adapter gets
                    // here.
                    let census = false;
                    if census {
                        let (_, name, desc) = method_key_parts(&thread.frames[*frame_idx]);
                        eprintln!(
                            "[cratonvm-osr] return-adapter {}{} entry_pc={} value={} adapters={}",
                            name,
                            desc,
                            entry_pc,
                            if osr_val.is_some() { "some" } else { "void" },
                            thread.continuation_return_adapters.len(),
                        );
                    }
                    let owed = match osr_val {
                        Some(value) => push_return_across_continuation_boundary(
                            shared, thread, *frame_idx, value, None,
                        ),
                        None => push_void_return_across_continuation_boundary(thread, *frame_idx),
                    };
                    if census {
                        eprintln!(
                            "[cratonvm-osr] return-adapter outcome={}",
                            match &owed {
                                Ok(()) => "converted",
                                Err(MethodCallFailed::ExceptionThrown(_)) => "threw",
                                Err(_) => "plain-return",
                            }
                        );
                    }
                    match owed {
                        Ok(()) => {
                            pop_and_recycle_frame(shared, thread);
                            *frame_idx -= 1;
                            return OsrBackoffOutcome::ContinueDispatch;
                        }
                        // Raised in the returning frame, as the `return`
                        // arms' `route_fast_path_error!` raises it.
                        // Wave 117 (lane L9 review): the record was emptied
                        // above for the compiled body's return; a handler of
                        // this frame may still run its `monitorexit`, so the
                        // empty record is marked understated where the method
                        // can hold a block monitor (`clear_untrusted_record`).
                        Err(MethodCallFailed::ExceptionThrown(exc)) => {
                            held_monitors::clear_untrusted_record(
                                &mut thread.frames[*frame_idx],
                                held_monitors::RecordProducer::OsrReturnAdapterThrow,
                            );
                            return OsrBackoffOutcome::ThrowJava(exc);
                        }
                        // Nothing was pushed: the plain return below.
                        Err(_) => {}
                    }
                }
                pop_and_recycle_frame(shared, thread);
                *frame_idx -= 1;
                if let Some(value) = osr_val {
                    thread.frames[*frame_idx].stack.push_unchecked(value);
                }
                OsrBackoffOutcome::ContinueDispatch
            } else {
                OsrBackoffOutcome::ReturnOuter(osr_val)
            }
        }
        None => {
            // An early offer's refusal is not one of the schedule's offers.
            if !early_entry {
                charge_osr_rejection(shared, thread, *frame_idx, entry_pc, 1);
            }
            OsrBackoffOutcome::Skip
        }
    }
}

/// MEGAOSR19-3's early window starts at this many back edges of the frame
/// (`CRATONVM_JIT_OSR_EARLY_REENTRY`). Above the handful an OSR entry costs
/// (cache lookup, seeding the locals, `validate_osr_entry`), well below
/// `OSR_THRESHOLD`; the book's range is 16-64.
pub(crate) const OSR_EARLY_REENTRY_BACKEDGES: u32 = 32;

/// MEGAOSR19-3 (JIT round 19 proposal wave 6, lane osrentry6;
/// `CRATONVM_JIT_OSR_EARLY_REENTRY`, default OFF): should this back edge be
/// offered OSR although `Frame::should_try_osr` says the schedule is not due?
///
/// `Frame::backward_count` is per activation, so a method invoked many times
/// with a short loop and no method-entry body interprets `OSR_THRESHOLD` back
/// edges in EVERY activation before it re-enters the OSR body an earlier
/// activation already caused to be compiled (the H2 reading of
/// `r19-optimizing-osr-body-loses-5x-at-blind-megamorphic-sites`). Yes when:
///
/// * the switch is on;
/// * the count is inside the early window, `[OSR_EARLY_REENTRY_BACKEDGES,
///   OSR_EARLY_REENTRY_BACKEDGES + OSR_POLL_STRIDE)` and below `threshold` --
///   under the poll point that is one ask per activation, since a declined
///   early offer moves the frame's next poll to `threshold`
///   (`try_osr_with_backoff`);
/// * the header has not been offered in this activation (no attempt charged);
/// * an enterable body is CACHED for `(method, entry_pc)`: a published
///   single-pass OSR artifact with an entry at this pc, or an accepted
///   optimizing OSR body (`jit_bridge::osr_optimizing_body_ready`). Nothing is
///   compiled or requested from here.
///
/// The offer then takes the ordinary path (agent / redefinition screens,
/// method-wide budget, `try_osr`'s reuse door), and `try_osr_offer` charges a
/// refused early entry to nothing.
pub(in crate::runtime::interpreter) fn osr_early_reentry_due(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    entry_pc: usize,
    threshold: u32,
) -> bool {
    if !crate::runtime::env_cache::osr_early_reentry() {
        return false;
    }
    let f = &thread.frames[frame_idx];
    if !osr_early_reentry_window(f.backward_count, threshold, f.osr_attempts_at(entry_pc)) {
        return false;
    }
    let enterable = osr_cached_body_enterable(shared, f, entry_pc);
    // OSRREENTRY21-3's census (j27 w6 lane T1OSRREENTRY): an ask that found
    // no body paid the cache lock and the optimizing memo's mutex for nothing.
    if !enterable {
        cratonvm_jit::metrics::record_osr_event("osr_early_window_miss");
    }
    enterable
}

/// OSRENTRY6-3 (JIT round 19 proposal wave 7, lane interp7;
/// `CRATONVM_JIT_OSR_EARLY_REQUEST`, default OFF, armed only with
/// `CRATONVM_JIT_OSR_EARLY_REENTRY`): should this early-window back edge,
/// which found NO enterable body cached ([`osr_early_reentry_due`] answered
/// `false`), request the OSR compile?
///
/// MEGAOSR19-3 only enters a body some earlier activation caused to be
/// compiled, i.e. one that once ran `threshold` back edges in ONE activation.
/// A method invoked many times whose loop never does (the BCEL
/// constant-pool shape, `R16FfmDowncallCost`) never gets one: its activations
/// end below the threshold and the per-frame count dies with each frame. Yes
/// when:
///
/// * both switches are on and background compilation is (the request is the
///   background arm's; nothing is ever compiled on the mutator from here);
/// * the count is in the early window ([`osr_early_reentry_window`], the same
///   one-ask-per-activation rule);
/// * the method's back edges summed over its interpreted activations at their
///   pops (`jit_bridge::note_osr_method_backedges`) reached `threshold` since
///   its last early request; answering `true` resets that sum, so concurrent
///   activations of one method ask once per `threshold` summed back edges.
///
/// The offer then takes `try_osr_offer`'s background arm: the agent and
/// redefinition screens, the method-wide budget, the OSR denial and refusal
/// arms (which retire the header as for a scheduled offer), and the request.
/// The next activation's early window enters the published body.
pub(in crate::runtime::interpreter) fn osr_early_request_due(
    shared: &SharedVm,
    thread: &JvmThread,
    frame_idx: usize,
    entry_pc: usize,
    threshold: u32,
) -> bool {
    if !osr_early_request_armed() || !crate::runtime::env_cache::bg_compile() {
        return false;
    }
    let f = &thread.frames[frame_idx];
    osr_early_reentry_window(f.backward_count, threshold, f.osr_attempts_at(entry_pc))
        && jit_bridge::take_osr_method_backedges(shared, frame_invoc_key(f), threshold)
}

/// `CRATONVM_JIT_OSR_EARLY_REENTRY_CREDIT` (JIT round 21, lane osrreentry;
/// perf, default ON, `=0` off): does an early OSR entry that ends its
/// activation credit the schedule's threshold of back edges
/// (`try_osr_offer`)? Read once per such entry, as
/// `CRATONVM_JIT_OSR_WAIT_CREDITS_LOOP_WORK` is read once per wait.
pub(in crate::runtime::interpreter) fn osr_early_reentry_credit_enabled() -> bool {
    true
}

/// [`osr_early_reentry_due`]'s count rule: inside the early window, below the
/// schedule's first due point, and no offer charged at this header yet.
#[inline]
pub(in crate::runtime::interpreter) fn osr_early_reentry_window(backward_count: u32, threshold: u32, attempts: Option<u32>) -> bool {
    backward_count >= OSR_EARLY_REENTRY_BACKEDGES
        && backward_count < threshold
        && backward_count < OSR_EARLY_REENTRY_BACKEDGES.saturating_add(OSR_POLL_STRIDE)
        && attempts.unwrap_or(0) == 0
}

/// Is an OSR body for `frame`'s method enterable at `entry_pc` from the cache
/// alone -- the two `reusable` answers `try_osr_offer`'s background arm asks,
/// without its compile request?
pub(in crate::runtime::interpreter) fn osr_cached_body_enterable(shared: &SharedVm, frame: &Frame, entry_pc: usize) -> bool {
    let (cn, mn, md, class_id) = (
        frame.class_name_arc(),
        frame.method_name_arc(),
        frame.method_descriptor_arc(),
        frame.class_id,
    );
    let single_pass = {
        let jc = shared.jit.jit_cache.read();
        matches!(
            jc.get_osr(&cn, &mn, &md, class_id),
            Some(c) if c.compiled_via_osr && c.can_osr_enter(entry_pc)
        )
    };
    single_pass
        || jit_bridge::osr_optimizing_body_ready(
            shared,
            class_id,
            &cn,
            &mn,
            &md,
            &frame.code,
            entry_pc,
        )
}

/// Charge `offers` refused OSR offers at `entry_pc` to the frame's budget
/// (`OSR_MAX_ATTEMPTS` retires the loop header for the activation), and when
/// that exhausts it, the activation to the method-wide record
/// (`jit_bridge::note_osr_refused_activation`) -- once per activation, on the
/// charge that reaches the cap.
pub(in crate::runtime::interpreter) fn charge_osr_rejection(
    shared: &SharedVm,
    thread: &mut JvmThread,
    frame_idx: usize,
    entry_pc: usize,
    offers: u32,
) {
    use crate::runtime::frame::OSR_MAX_ATTEMPTS;
    let frame = &mut thread.frames[frame_idx];
    let before = frame.osr_attempts_at(entry_pc).unwrap_or(0);
    if before >= OSR_MAX_ATTEMPTS {
        return;
    }
    let after = before.saturating_add(offers).min(OSR_MAX_ATTEMPTS);
    frame.set_osr_attempts(entry_pc, after);
    if after == OSR_MAX_ATTEMPTS {
        jit_bridge::note_osr_refused_activation(shared, &thread.frames[frame_idx], entry_pc);
    }
}


// ---------------------------------------------------------------------------
// Main execution loop
// ---------------------------------------------------------------------------

/// Fetch (building on first use) the pre-decoded instruction stream for the
/// method this frame is executing.
///
/// Two tiers, both one-time-per-method:
///
/// * `Cached` frames read a `OnceLock` on the shared `CachedBytecodeMethod`
///   -- an acquire load after the first call.
/// * `Owned` frames (non-cached invoke paths, synthetic frames, JNI entry
///   stubs) go to the process-wide intern table, which is keyed on the
///   identity of the bytecode allocation so both tiers share one stream per
///   method.
///
/// `None` means the method is not quickenable and the caller must keep using
/// `Instruction::decode`.
///
/// A frame running a replaced body whose `ldc` names a constant above index
/// 255 carries its own stream (`Frame::widened_stream`, interpreter round i1
/// wave 21, lane L4): the intern table would decode that `ldc`'s stale
/// operand byte, and evicts its entries, so the frame's stream is not there.
/// Every other frame moved onto a replaced body keeps its stream with that
/// body too (`ReplacedBody::decoded_stream`, wave 25, lane L3): an intern
/// entry would hold the frame's private code copy until its shard is cleared.
#[inline]
pub(in crate::runtime::interpreter) fn quickened_for_frame(frame: &Frame) -> Option<Arc<cratonvm_reader::QuickenedCode>> {
    match frame.cached_method() {
        Some(cm) => cm
            .quickened
            .get_or_init(|| cratonvm_reader::quickened::intern(&cm.code))
            .clone(),
        None => match frame.replaced_body() {
            Some(replaced) => replaced.decoded_stream(&frame.code),
            None => cratonvm_reader::quickened::intern(&frame.code),
        },
    }
}

pub(in crate::runtime::interpreter) fn execute_frame(shared: &SharedVm, thread: &mut JvmThread) -> MethodCallResult {
    let initial_frame_idx = thread.frames.len() - 1;
    execute_frame_from_index(shared, thread, initial_frame_idx)
}

/// How a fast-path invoke dispatch failure should re-enter the main loop.
pub(in crate::runtime::interpreter) enum FastPathInvokeError {
    /// Convert through `throw_runtime_error` at the top of the loop.
    Runtime(RuntimeError),
    /// Already a Java throwable — route it through the exception table.
    Java(ObjectRef),
    /// Not expressible as a Java throwable; unwind the whole invocation.
    Fatal(MethodCallFailed),
}

/// Classify an error returned by one of the stackless invoke fast paths
/// (`0xb6`/`0xb7`/`0xb8`/`0xb9`).
///
/// Those arms handle their own errors and `continue`, so they never reach the
/// per-opcode conversion that guards the slow path at the bottom of the loop —
/// they have to perform the same conversions themselves. `VmError::Linkage`
/// was the one they did not: every `java.lang.LinkageError` subclass is an
/// ordinary throwable (JVMS §5.4), but the fast paths returned it raw, which
/// skipped every exception handler in every frame and killed the process at
/// `main-vm run()`.
///
/// Concretely: `ClassLoader.defineClass1` rejecting a bad-magic class file is
/// reached by `invokestatic` from `ClassLoader.defineClass`, so
/// `assertThatExceptionOfType(ClassFormatError.class)` in
/// `RestartClassLoaderTests.getUpdatedClass` aborted the VM instead of
/// passing, and a `catch (Throwable)` wrapped directly around `defineClass`
/// never ran. `probes/DefineClassFormatErrorProbe.java` and
/// `probes/DefineClassWhereLostProbe.java` cover both shapes.
///
/// Every `Java` answer has been through [`settle_thrown_exception`] — the
/// repair the decoded path's `ExceptionThrown` arm applies to every throw
/// (drain `native_pending_return`; prefer it only when the thrown address is
/// no longer a live object). Until wave 5 only the `athrow` and
/// `invokedynamic` arms applied it, by hand, and the other twenty call sites
/// left a native's leftover return value rooted past the throw. Doing it
/// here, in the one function every raw-bytecode arm's error goes through,
/// means a new call site cannot forget it.
#[cold]
pub(in crate::runtime::interpreter) fn classify_fastpath_invoke_error(
    shared: &SharedVm,
    thread: &mut JvmThread,
    err: MethodCallFailed,
) -> FastPathInvokeError {
    if dbg_linkage() {
        if let MethodCallFailed::InternalError(VmError::Linkage(l)) = &err {
            dbg_linkage_dump(thread, "fast-path invoke", &format!("{l:?}"));
        }
    }
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(re)) => FastPathInvokeError::Runtime(re),
        MethodCallFailed::ExceptionThrown(exc) => {
            FastPathInvokeError::Java(settle_thrown_exception(shared, thread, exc))
        }
        MethodCallFailed::InternalError(VmError::Linkage(linkage_err)) => {
            match crate::runtime::exceptions::throw_linkage_error(shared, thread, linkage_err) {
                MethodCallFailed::ExceptionThrown(exc) => {
                    FastPathInvokeError::Java(settle_thrown_exception(shared, thread, exc))
                }
                // The throwable could not be built (`throw_linkage_error` logs
                // why). Preserve the old behaviour rather than losing the error.
                other => FastPathInvokeError::Fatal(other),
            }
        }
        other => FastPathInvokeError::Fatal(other),
    }
}

/// What the dispatch loop does after [`loop_top_poll`]. `word_gate` is the
/// poll word with its pause count masked off, the loop's new snapshot of it.
pub(in crate::runtime::interpreter) enum LoopTopPoll {
    /// Nothing more: dispatch the bytecode.
    Settled { word_gate: u32 },
    /// A frame of this stack moved onto another code allocation: refresh the
    /// fast-path gate against the stack's new move count `moves`.
    FramesMoved { moves: u32, word_gate: u32 },
    /// A deferred frame conversion was retried: start the iteration over.
    RetriedConversion,
}

/// The dispatch loop's slow path, taken when this OS thread's poll word
/// (`gc_barrier::LoopPollWord`) differs from the loop's snapshot of it
/// (interpreter round i1 wave 26, lane L7). Answers, in the order the loop top
/// used to test them inline:
///
/// 1. a pause in progress (the word's pause count, which the barrier keeps
///    for exactly as long as `stw_requested` is up): `safepoint_check` when
///    `stw_flag` -- this VM's authoritative flag, read with `Acquire` after an
///    `Acquire` read of the word -- is up. The count stays until the pause
///    ends, so the loop comes back here at every bytecode while it lasts, as
///    it called `safepoint_check` while it polled the flag;
/// 1a. a class redefinition since the thread last looked
///    (`obsolete_frames::convert_at_loop_top`, wave 37): its frames move, or
///    a redefinition fence defers the conversion, which step 2 retries;
/// 2. the stack's own move count against `gate_moves` (read after the
///    safepoint, which may have moved frames): a pending deferred conversion
///    is retried first (`obsolete_frames::retry_deferred_conversion`), else
///    the loop refreshes its gate. The word only TRIGGERS this compare -- a
///    move bumps the word of the OS thread it runs on, which may be another
///    stack's move on the same thread (a carrier's own loop under a mounted
///    virtual thread's), and then nothing moved here.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn loop_top_poll(
    shared: &SharedVm,
    thread: &mut JvmThread,
    stw_flag: &std::sync::atomic::AtomicBool,
    poll: &LoopPollWord,
    gate_moves: u32,
) -> LoopTopPoll {
    // One `Acquire` read of the word, first: it synchronizes with the
    // barrier's pause count and with a handshake requester's kick (both
    // `Release` adds), so what they published before is visible below.
    let word = poll.load_acquire();
    // gcj j1/cm (P4/ph of `common-a-proposal-per-thread-handshakes`): the
    // handshake poll. A frame-trace requester (`vm_exec::frame_trace_by_handshake`,
    // opt-in `CRATONVM_GC_HANDSHAKE_FRAME_TRACE`) queues its op and then
    // kicks every loop's word, so a loop that allocates nothing serves it
    // here at its next bytecode. One `Acquire` load of this thread's queue
    // word when nothing is queued (always, with the switch off); this path is
    // cold. Before the pause branch, as in `safepoint_check`.
    //
    // t1 lane E (`CRATONVM_GC_HANDSHAKE_POLL_BIT`, opt-in): a requester that
    // armed this word's HANDSHAKE bit (in the byte masked off the gate, so
    // the loop comes here until it is lowered) is answered by lowering it
    // FIRST and draining after: an arm ordered before the clear has its op
    // visible to the drain, one after it keeps the bit up for the next top.
    if word & LoopPollWord::HANDSHAKE != 0 {
        poll.take_handshake_arm();
    }
    thread.run_pending_handshakes(shared);
    if word & LoopPollWord::PAUSES != 0 && stw_flag.load(std::sync::atomic::Ordering::Acquire) {
        safepoint_check(shared, thread);
    }
    // A class redefinition since this thread last looked: move its frames now,
    // or wait at this top while a redefinition fence holds it (interpreter
    // round i1 wave 37, lane L3; the redefinition arms every loop's word). A
    // pending conversion is retried below instead.
    if !thread.frames.conversion_pending() {
        obsolete_frames::convert_at_loop_top(shared, thread);
    }
    let word_gate = poll.load() & !LoopPollWord::PAUSES;
    let moves = thread.frames.code_moves();
    if moves == gate_moves {
        return LoopTopPoll::Settled { word_gate };
    }
    if thread.frames.conversion_pending() {
        obsolete_frames::retry_deferred_conversion(shared, thread);
        return LoopTopPoll::RetriedConversion;
    }
    LoopTopPoll::FramesMoved { moves, word_gate }
}


/// The fast value-return arm's operand-stack underflow (`ireturn` ..
/// `areturn` with an empty stack, which valid bytecode cannot reach): name
/// the method and return the error.
///
/// Out of line, and handed `saved_pc` and `opcode` BY VALUE, for the dispatch
/// loop's code generation (interpreter round i1 wave 27, lane L7). This used
/// to be an inline `eprintln!` / `format!` of the two locals, and a format
/// argument is a BORROW (`format_args!` takes `&opcode`), which rustc
/// answers by giving the borrowed local a stack slot for the whole function
/// instead of an SSA value, and the borrow escapes into the formatter, so
/// LLVM cannot promote it back. Every bytecode then stored its opcode byte to
/// that slot right after fetching it (`mov %cl,0x347(%rsp)` in the fat-LTO
/// profile of wave 26, 5-6% of the loop's samples) and the arms that test
/// `opcode` again read it back from memory. The same holds for every other
/// diagnostic below that names `saved_pc`: see [`decoded_frame_out_of_range`],
/// [`no_instruction_decoded`] and [`decoded_error_tripwires`]. Nothing in
/// `execute_frame_from_index` may take `&saved_pc`, `&opcode`, `&b1` or `&b2`
/// (a `format!`, `eprintln!`, `trace!` argument, a non-`move` closure, a
/// `contains(&b1)`): pass a copy, or call out of line with the value.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn value_return_underflow(
    shared: &SharedVm,
    frame: &Frame,
    saved_pc: usize,
    opcode: u8,
) -> MethodCallFailed {
    let mname = frame.method_name().to_string();
    let mdesc = frame.method_descriptor().to_string();
    let cname = shared
        .classes
        .class_manager
        .read()
        .get_class(frame.class_id)
        .map(|c| c.name.to_string())
        .unwrap_or_default();
    eprintln!(
        "[cratonvm] operand-stack underflow at value return: {}.{}{} pc={} opcode=0x{:x}",
        cname, mname, mdesc, saved_pc, opcode
    );
    MethodCallFailed::InternalError(VmError::Internal {
        message: format!(
            "operand-stack underflow at value return in {cname}.{mname}{mdesc} pc={saved_pc}"
        ),
    })
}

/// The decoded path's frame-index check failed (see
/// [`value_return_underflow`] for why this is out of line).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn decoded_frame_out_of_range(frame_idx: usize, depth: usize, saved_pc: usize) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Internal {
        message: format!("frame index {frame_idx} out of range (depth {depth}) at pc={saved_pc}"),
    })
}

/// The dispatch loop's hoisted frame pointer again, after a loop-top hook
/// that reborrowed `thread.frames` (the JVMTI single-step hook) or parked
/// where Java may run on this thread and push frames (a breakpoint): the
/// match turns it into its `&mut Frame` (`FrameStack::frame_mut_from_ptr`).
/// Out of line and cold, like the hooks that need it (interpreter round i1
/// wave 37, lane L7).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn rederive_hot_frame_ptr(
    thread: &mut JvmThread,
    frame_idx: usize,
) -> Result<*mut Frame, MethodCallFailed> {
    let p = thread.frames.frame_ptr(frame_idx);
    if p.is_null() {
        return Err(MethodCallFailed::InternalError(VmError::Internal {
            message: format!(
                "frame index {frame_idx} out of range (depth {}) after a loop-top hook",
                thread.frames.len()
            ),
        }));
    }
    Ok(p)
}

/// The decoded path found no instruction at `saved_pc` (unreachable by
/// construction; see its call site, and [`value_return_underflow`] for why
/// this is out of line).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn no_instruction_decoded(saved_pc: usize) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Internal {
        message: format!("no instruction decoded at pc={saved_pc}"),
    })
}

/// The decoded path's two error tripwires, run when `execute_instruction`
/// failed. Moved verbatim out of `execute_frame_from_index` in wave 27 (see
/// [`value_return_underflow`] for why).
///
/// DIAG (gated `CRATONVM_DBG_UNDERFLOW=1`): pinpoint an operand-stack
/// underflow — log the offending method/bci/opcode + the Java frame chain the
/// first time one surfaces, so a mis-modelled bytecode path can be minimized
/// without a full TestAll run. (The H2 TestAll underflow is no longer
/// reproducible after the toArray(T[]) template-type fix; this is a low-cost
/// tripwire — it only runs on the rare IllegalState error path.)
///
/// DIAG (gated `CRATONVM_DBG_POPINT=1`): pinpoint a `pop_int` type mismatch
/// ("expected int on stack, got ref(...)") — log the offending
/// method/bci/opcode + the Java frame chain the first time one surfaces.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn decoded_error_tripwires(
    thread: &JvmThread,
    frame_idx: usize,
    saved_pc: usize,
    instruction: &Instruction,
    exec_result: &Result<InstructionResult, MethodCallFailed>,
) {
    if let Err(MethodCallFailed::InternalError(VmError::Runtime(
        RuntimeError::IllegalStateException { message },
    ))) = exec_result
    {
        if message == "operand stack underflow"
            && false
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static FIRED: AtomicBool = AtomicBool::new(false);
            if !FIRED.swap(true, Ordering::Relaxed) {
                let f = &thread.frames[frame_idx];
                eprintln!(
                    "[DBG_UNDERFLOW] at {}.{}{} bci={} opcode={:?} stack_len={}",
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    saved_pc,
                    instruction,
                    f.stack.len(),
                );
                for (i, fr) in thread.frames.iter().enumerate().rev() {
                    eprintln!(
                        "    [{}] {}.{}{} pc={}",
                        i,
                        fr.class_name(),
                        fr.method_name(),
                        fr.method_descriptor(),
                        fr.pc
                    );
                }
            }
        }
    }

    if let Err(MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NotImplemented {
        feature,
    }))) = exec_result
    {
        if feature.starts_with("expected int on stack, got")
            && false
        {
            use std::sync::atomic::{AtomicBool, Ordering};
            static FIRED_POPINT: AtomicBool = AtomicBool::new(false);
            if !FIRED_POPINT.swap(true, Ordering::Relaxed) {
                let f = &thread.frames[frame_idx];
                eprintln!(
                    "[DBG_POPINT] {} at {}.{}{} bci={} opcode={:?} stack_len={}",
                    feature,
                    f.class_name(),
                    f.method_name(),
                    f.method_descriptor(),
                    saved_pc,
                    instruction,
                    f.stack.len(),
                );
                for (i, fr) in thread.frames.iter().enumerate().rev() {
                    eprintln!(
                        "    [{}] {}.{}{} pc={}",
                        i,
                        fr.class_name(),
                        fr.method_name(),
                        fr.method_descriptor(),
                        fr.pc
                    );
                }
            }
        }
    }
}

/// `CRATONVM_TRACE_SB_FILTER`: the fast value-return arm's report of the
/// `value` a Spring Boot auto-configuration filtering method (`frame`)
/// returns.
///
/// Moved verbatim out of `execute_frame_from_index` in interpreter round i1
/// wave 27 (lane L7), and handed `value` BY VALUE: inline it formatted the
/// arm's `value` (`match &value`, `{:?}`), a borrow that escapes into the
/// formatter, so every interpreted value return had to materialize its
/// `Value` in a stack slot for a trace nobody armed (see
/// [`value_return_underflow`]).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn trace_sb_filter_return(shared: &SharedVm, frame: &Frame, value: Value) {
    let cn = frame.class_name();
    let mn = frame.method_name();
    let interesting = (cn.contains("FilteringSpringBootCondition") && mn == "match")
        || (cn.contains("ImportCandidates")
            && (mn == "getCandidates"
                || mn == "readCandidateConfigurations"
                || mn == "load"
                || mn == "stripComment"))
        || (cn.contains("AutoConfigurationImportSelector")
            && (mn == "removeDuplicates"
                || mn == "getCandidateConfigurations"
                || mn == "getAutoConfigurationImportFilters"
                || mn == "getExclusions"))
        || (cn.contains("ConfigurationClassFilter") && mn == "filter")
        || (cn.contains("AutoConfigurationEntry") && mn == "getConfigurations");
    if interesting {
        let extra = match &value {
            Value::Object(Some(o)) => {
                let r = *o;
                let cid = shared.mem.heap.class_id_of(r);
                let kind = shared.mem.heap.kind_of(r);
                // Try to read as String first
                if let Some(s) = crate::vm::read_java_string(&shared.mem.heap, r) {
                    format!(" cid={:?} kind={:?} STRING=\"{}\"", cid, kind, s)
                } else if matches!(kind, crate::memory::heap::ObjectKind::Object) {
                    // Read ArrayList-style 'size' field heuristically — sample
                    // field 0..3 to find any Int-typed field
                    let mut fields_desc = String::new();
                    for fi in 0..6usize {
                        let v = shared.mem.heap.get_field(r, fi);
                        fields_desc.push_str(&format!("f{}={:?} ", fi, v));
                    }
                    format!(" cid={:?} kind={:?} fields=[{}]", cid, kind, fields_desc)
                } else {
                    // Try array
                    let len = shared.mem.heap.array_length(r);
                    let mut samples = String::new();
                    let to_read = len.min(20);
                    for i in 0..to_read {
                        match shared.mem.heap.get_array_element(r, i) {
                            Ok(Value::Int(v)) => samples.push_str(&format!("{},", v)),
                            Ok(Value::Object(Some(oo))) => {
                                if let Some(s) = crate::vm::read_java_string(&shared.mem.heap, oo) {
                                    samples.push_str(&format!("\"{}\",", s));
                                } else {
                                    samples.push_str("O,");
                                }
                            }
                            Ok(Value::Object(None)) => samples.push_str("N,"),
                            _ => samples.push('?'),
                        }
                    }
                    format!(
                        " cid={:?} kind={:?} array_len={} samples=[{}]",
                        cid, kind, len, samples
                    )
                }
            }
            _ => String::new(),
        };
        eprintln!("[SBF-RET] {}.{} -> {:?}{}", cn, mn, value, extra);
    }
}

/// The T19.H1 stack-dump hook's body, out of line (interpreter round i3 wave
/// 83, lane L7, host frame rework): the dump, then the sampling-mode request
/// clear. Returns whether the frame's at-most-once guard
/// (`stack_dump_emitted`) is now set: `true` outside sampling mode, `false`
/// in it (the request was consumed instead). Wave 82 wrote the whole body,
/// the `dump_current_thread_frames` call included, as a macro expanded at
/// two sites of the loop (the per-bytecode test and the poll's slow path).
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn serve_stack_dump(shared: &SharedVm, thread: &JvmThread) -> bool {
    shared.dump_current_thread_frames(thread);
    if shared.stack_sample_mode() {
        shared.clear_stack_dump_request();
        false
    } else {
        true
    }
}

/// The dispatch loop's parked fast-path `RuntimeError` and the pc it is
/// raised at (interpreter round i4 wave 84, lane L7, frame ratchet).
///
/// Until wave 84 the loop carried `Option<(RuntimeError, usize)>` inline:
/// `RuntimeError`'s largest variant (`PatternSyntaxException`, two `String`s
/// and an `i32`) makes that 64 bytes, and a loop-carried local keeps its own
/// slot for the whole function (its alloca's lifetime spans every iteration,
/// so stack colouring cannot share it with the arms' temporaries). Boxed it
/// is one pointer; the loop top's per-bytecode test is the same null compare
/// it was a discriminant compare before. The allocation happens only when an
/// arm parks an error -- an AIOOBE / NPE / ASE / OOME that is about to build
/// a throwable anyway -- in [`park_runtime_error`], out of line.
pub(in crate::runtime::interpreter) type PendingRuntimeError = Option<Box<(RuntimeError, usize)>>;

/// Park `re`, raised at `pc`, for the loop top (see [`PendingRuntimeError`]).
/// Cold and out of line so each parking site stays a call with its operands.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn park_runtime_error(re: RuntimeError, pc: usize) -> PendingRuntimeError {
    Some(Box::new((re, pc)))
}


/// Execute a previously frozen stack. `initial_frame_idx` is the oldest frame
/// owned by this invocation, while dispatch resumes at the current top frame.
///
/// Interpreter round i6 wave 116, lane L8 (`CRATONVM_INTERP_LEAN_LOOP_TOP`,
/// `VmConfig::interp_lean_loop_top`, default off): the loop exists in two
/// monomorphic instances, and the switch is read here, once per entry, never
/// per bytecode. `LEAN_TOP = false` is the loop as it was; `true` folds the
/// loop top's three `top_gates` tests into one and keeps the code length
/// cached at the frame-switch compare (see the loop top). Each instance is
/// the whole loop, so the binary carries the loop twice.
#[inline]
pub(in crate::runtime::interpreter) fn execute_frame_from_index(
    shared: &SharedVm,
    thread: &mut JvmThread,
    initial_frame_idx: usize,
) -> MethodCallResult {
    if shared.config.interp_lean_loop_top {
        execute_frame_from_index_loop::<true>(shared, thread, initial_frame_idx)
    } else {
        execute_frame_from_index_loop::<false>(shared, thread, initial_frame_idx)
    }
}

