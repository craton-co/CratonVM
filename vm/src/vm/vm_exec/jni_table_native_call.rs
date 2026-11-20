#[allow(unused_imports)]
use super::*;

/// The table-bound JNI arm of [`invoke_on_class_shared_inner`]
/// (`find_jni_native` answered `fn_ptr`: `RegisterNatives`, or a symbol the
/// `dlsym` arm bound on an earlier call): the JVMTI row and the debugger's
/// entry report, the JNI context, the implicit local frame, the arity check,
/// the census, the C call, the pending-exception check and the exit report.
///
/// gci i8/dm moved this body out of the function UNCHANGED (only dedented),
/// so that a hit of the per-thread JNI dispatch memo
/// ([`jni_dispatch_memo_hit`]) runs exactly the code the full path runs. The
/// caller holds a synchronized method's monitor
/// ([`class_shared_sync_monitor`]) across the call, as before; `class_name`
/// is the DECLARING class's name.
#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn jni_table_native_call(
    shared: &SharedVm,
    thread: &mut JvmThread,
    fn_ptr: usize,
    declaring_class_id: ClassId,
    is_static: bool,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> MethodCallResult {
    let jni_census = &shared.natives.jni_phase_census;
    let mut jni_clock = jni_census.start();
    // Interpreter round i1 wave 42 (lane L1): the native's row for the
    // JVMTI stack functions, which list it at location -1 while it runs
    // (`jvmti::native_env::NativeFrameRow`), its method-event reports
    // included; popped when the arm's scope ends, on every exit edge.
    // Keyed by `fn_ptr`, plain data: the method is named only when a
    // stack function reads the row (one `Vec` push per call).
    // Safety: `thread` is the live `&mut JvmThread` of this dispatch
    // and outlives the row.
    // Only `experimental-debug` builds have a JVMTI C table to read the
    // row, so release builds record nothing here.
    #[cfg(feature = "experimental-debug")]
    let _jvmti_native_row = unsafe {
        crate::jvmti::native_env::NativeFrameRow::enter_jni(
            thread as *mut _,
            declaring_class_id,
            fn_ptr,
        )
    };
    // Interpreter round i1 wave 28 (lane L1): the JDWP / JVMTI
    // `MethodEntry` of a JNI-bound native, which never reaches the
    // native-call funnel's report — one relaxed load without a debugger
    // or an agent, in a build with a JDWP server only. Reported before
    // the JNI context and the implicit local frame are installed; a
    // suspending request parks the thread there, and the object
    // arguments are then re-read from the pins the report took.
    #[cfg(feature = "experimental-debug")]
    let jni_debugged = if shared.debug.debugger_gates.method_events_armed() {
        crate::runtime::interpreter::report_jni_native_method_entry(
            shared,
            thread,
            declaring_class_id,
            method_name,
            descriptor,
            args,
        )
    } else {
        None
    };
    #[cfg(feature = "experimental-debug")]
    let args: &[Value] = match jni_debugged.as_ref() {
        Some((_, Some(fresh))) => fresh.as_slice(),
        _ => args,
    };
    // JNI function pointer registered via RegisterNatives or symbol lookup.
    // Set TLS context so that JNI callbacks (e.g. FindClass, CallMethod)
    // can access the VM from within the native library.
    //
    // SECURITY: install the context through an RAII guard rather than a
    // manual set/clear pair. The guard's `Drop` RESTORES the TLS pointers
    // on EVERY exit edge — the arity-mismatch early return, the normal
    // return, and (critically) an unwind out of `dispatch_jni_native`.
    // The previous manual-clear pattern skipped the clears on panic,
    // leaving dangling `*mut JvmThread` / `*mut SharedVm` pointers in TLS
    // for a later JNI up-call to dereference (use-after-free).
    //
    // It restores rather than clears because this dispatch NESTS: a
    // native that calls Java that calls another native gets two guards,
    // and the inner one's exit must not leave the outer native without a
    // context. See `JniContextGuard::install` for the measurement.
    //
    // Safety: `thread` is the live, exclusively-borrowed `&mut JvmThread`
    // for this dispatch; it outlives the guard (and thus the whole native
    // call) per `set_jni_thread`'s contract.
    let _jni_guard = unsafe {
        JniContextGuard::install_for_native(shared, thread as *mut _, declaring_class_id)
    };
    jni_census.lap(crate::native::jni::JniPhase::CtxInstall, &mut jni_clock);

    // GC-correctness (vm-jni-roots #2): bracket the native call in an
    // implicit local-ref frame so any local jobject the native creates
    // is a GC root for the call's lifetime. Dropped (frame popped) on
    // every exit below — normal return, arity-mismatch early return,
    // and panic unwind through the unsafe dispatch.
    let _jni_local_frame = JniImplicitFrameGuard::enter();
    jni_census.lap(crate::native::jni::JniPhase::LocalFramePush, &mut jni_clock);

    let env = crate::native::jni::get_jni_env();
    // For instance methods, args[0] is the receiver; for static, it is absent.
    // A static native's second C parameter is the `jclass` of the
    // class that DECLARES it — that is what `GetStaticMethodID(env,
    // cls, ...)`, `GetStaticFieldID`, `FindClass`-free upcalls and
    // `RegisterNatives`-style self-reference are all written against.
    // Passing `0` there made every one of those calls fail: the strict
    // corpus's `callBackTriple` returned its own "GetStaticMethodID
    // returned NULL" sentinel (-1) where HotSpot returned 27.
    //
    // The encoding is the one the rest of this JNI table uses for a
    // `JClass`: `class_id_to_jclass`, exactly what `FindClass` hands
    // back. It must go through that helper and not `as u32`: a bare
    // `ClassId` of 0 IS JNI NULL, and an untagged one is not
    // recognisable as a class by `NewGlobalRef`. JNA's `initIDs` does
    // `NewGlobalRef(cls)` on precisely this argument and reported
    // `UnsatisfiedLinkError: Can't obtain global reference for class
    // com.sun.jna.Native` when the two encodings disagreed.
    let (receiver, call_args) = if is_static {
        (
            crate::native::jni::class_id_to_jclass(declaring_class_id),
            args,
        )
    } else {
        let recv = match args.first() {
            Some(Value::Object(Some(r))) => crate::native::jni::obj_to_jobject(*r),
            _ => 0u64,
        };
        (recv, if args.is_empty() { args } else { &args[1..] })
    };

    // V2: the descriptor comes from (untrusted) classfile data while
    // `call_args` is the actual operand-stack frame. `dispatch_jni_native`
    // `zip`s the two together, so a descriptor whose parameter count
    // disagrees with `call_args.len()` silently builds a malformed C
    // call frame (missing/extra register args) — UB inside the unsafe
    // dispatch. Reject the mismatch with UnsatisfiedLinkError before the
    // unsafe call instead of entering it with a bad frame. The
    // `_jni_guard` `Drop` clears the TLS context on this early return.
    let expected_params = crate::runtime::proxy::count_descriptor_params(descriptor);
    if expected_params != call_args.len() {
        tracing::warn!(
            method = %format!("{class_name}.{method_name}{descriptor}"),
            expected_params,
            got_args = call_args.len(),
            "JNI native arity mismatch — refusing unsafe dispatch"
        );
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::UnsatisfiedLinkError {
                message: format!(
                    "{class_name}.{method_name}{descriptor}: descriptor arity {expected_params} != {} args",
                    call_args.len()
                ),
            },
        )));
    }

    // §4 census — the JNI half. `record_invocation` cannot serve this
    // edge (it keys on a `NativeMethodId`, and only
    // `NativeMethodRegistry` issues one; a `dlsym` result has none), so
    // the count lives on the table's own realm and the report adds it
    // to `bridge_invocations`. Counted here, after the arity check, so
    // a refused unsafe dispatch is not counted as one.
    shared
        .natives
        .jni_bridge_invocations
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // gcd d4/k: the JNI census of the native-transition flip gate --
    // `RUST_LOG=cratonvm::jni_census=debug` names every JNI native a
    // workload dispatches (`gcd-d2i-jni-native-methods-are-counted-mutators`).
    tracing::debug!(
        target: "cratonvm::jni_census",
        class = %class_name,
        method = %method_name,
        descriptor = %descriptor,
        "JNI native dispatch (registered)"
    );
    // A JNI native is a native frame a virtual thread's continuation
    // cannot unwind through: counted like a registered native, so a
    // park under a JNI upcall pins (wave 26, lane L4;
    // `native_callee_memo::continuation_pinned_by_native`).
    let _jni_native_frame = crate::runtime::native_callee_memo::enter_native();
    jni_census.lap(crate::native::jni::JniPhase::DoorPrologue, &mut jni_clock);
    // The i53 retirement: listed while its C code runs, as a
    // registered native is (`enter_jni_native`).
    let jni_span = enter_jni_native(shared, thread);
    // Safety: fn_ptr was stored from a trusted RegisterNatives / JNI_OnLoad call.
    let result_value = {
        // Interpreter round i2 wave 58, lane L1: the C JVMTI table's
        // in-native mark for the C call (one test and one load
        // unless a C agent is bound and `CRATONVM_JVMTI_C_IN_NATIVE`
        // is on). Safety: `thread` is this dispatch's live thread.
        #[cfg(feature = "experimental-debug")]
        let _c_in_native = unsafe {
            crate::jvmti::native_env::CInNativeMark::enter(shared, thread as *mut _)
        };
        unsafe {
            crate::native::jni::dispatch_jni_native_census(
                fn_ptr,
                env,
                receiver,
                call_args,
                descriptor,
                Some(jni_census),
            )
        }
    };
    leave_jni_native(shared, thread, jni_span);
    jni_census.lap(crate::native::jni::JniPhase::Dispatch, &mut jni_clock);

    // `_jni_guard` restores the previous TLS context when it drops at end
    // of scope. void methods return Value::Object(None) here.
    let ret_char = descriptor
        .rfind(')')
        .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
        .unwrap_or(b'V');
    #[cfg_attr(not(feature = "experimental-debug"), allow(unused_mut))]
    let mut out = match jni_pending_exception_after_native(shared, thread) {
        Some(failed) => Err(failed),
        None if ret_char == b'V' => Ok(None),
        None => Ok(Some(result_value)),
    };
    jni_census.stop(crate::native::jni::JniPhase::ExceptionCheck, jni_clock);
    // Wave 28: the native's `MethodExit`, with the thread's ordinary
    // state back first (a suspending request parks here, and the
    // report pins the result or the exception across the park).
    // Wave 45 (lane L1): then, a thread the debugger suspended while
    // the native ran parks here with the native on top
    // (`interpreter::park_if_suspended_at_native_exit`).
    #[cfg(feature = "experimental-debug")]
    {
        let reported = jni_debugged
            .as_ref()
            .map(|(native, _)| native)
            .filter(|native| native.reports_exit(&out));
        let suspension = shared.debug.debugger_gates.suspension_in_force();
        if reported.is_some() || suspension {
            drop(_jni_native_frame);
            drop(_jni_local_frame);
            drop(_jni_guard);
            if let Some(native) = reported {
                crate::runtime::interpreter::report_native_method_exit(
                    shared, thread, native, &mut out,
                );
            }
            if suspension {
                crate::runtime::interpreter::park_if_suspended_at_native_exit(
                    shared,
                    thread,
                    crate::runtime::interpreter::ReturningNative::Named(
                        u64::from(declaring_class_id.as_u32()),
                        crate::debug::jdwp_method_id(method_name, descriptor),
                    ),
                    &mut out,
                );
            }
        }
    }
    out
}

/// Take a synchronized callee's monitor for an arm of
/// `invoke_on_class_shared_inner` that answers the call itself (a native).
/// The bytecode tail does not come here unless `CRATONVM_NO_EXECUTE_SYNC` is
/// set: `interpreter::execute` takes the monitor itself.
///
/// `args` is the function's pinned dispatch vector (`args_roots` pins it). It
/// is refreshed after the mirror fetch, which can allocate, and remapped in
/// place across a contended wait, so the arm that runs next reads current
/// addresses.
pub(in crate::vm::vm_exec) fn class_shared_sync_monitor(
    shared: &SharedVm,
    thread: &mut JvmThread,
    declaring_class_id: ClassId,
    is_static: bool,
    args_roots: &PinnedDispatchArgs,
    args: &mut [Value],
) -> Result<SynchronizedMethodGuard, MethodCallFailed> {
    let monitor = if is_static {
        // Static synchronized: the monitor is the class's `Class` mirror
        // (JVMS §2.11.10) — the SAME object user code locks via
        // `synchronized (X.class)` and signals via `X.class.wait()/notify()`.
        // A separate synthetic lock object broke `Class.notifyAll()` from a
        // static synchronized method (IllegalMonitorStateException), which
        // aborted the real Keycloak/Quarkus boot at
        // `ApplicationStateNotification.notifyStartupFailed`.
        // `args` is pinned (`args_roots`), so the mirror may collect: on a
        // full heap its first use throws `OutOfMemoryError` instead of
        // aborting (gen r5w1/oom5).
        let mirror = crate::runtime::interpreter::constants::class_mirror_or_oom(
            shared,
            thread,
            declaring_class_id,
            "static-synchronized-monitor",
        );
        args_roots.refresh(args);
        mirror?
    } else {
        args_roots.refresh(args);
        match args.first() {
            Some(Value::Object(Some(obj_ref))) => *obj_ref,
            // HotSpot's monitor enter on a null `this` is a catchable
            // `NullPointerException`, not a VM-internal failure.
            Some(Value::Object(None)) => {
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
            _ => {
                return Err(MethodCallFailed::InternalError(VmError::Internal {
                    message: "synchronized instance method called with null or missing this"
                        .to_string(),
                }));
            }
        }
    };
    Ok(SynchronizedMethodGuard::enter(
        shared, thread, monitor, args,
    ))
}

/// Turn an exception a JNI native left PENDING into this call's failure.
///
/// `ThrowNew`/`Throw` do not unwind — they record a pending throwable and let
/// the native run to its `return`, and the exception becomes real to Java at
/// the moment the native returns. Only the Rust-registry dispatch path
/// (`safe_native_call`) was draining that slot; the two JNI function-pointer
/// paths below returned the native's value and left the throwable sitting in
/// thread-local storage for whatever ran next to pick up.
///
/// The strict corpus caught the shape exactly, because it records each branch
/// separately rather than printing one verdict:
///
/// ```text
/// upcall=-1 throw=<no-throw> throw=ISE:from-native registered=false
/// ```
///
/// BOTH branches printed — the `try` body completed (so the native "returned
/// normally") and the `catch` also ran, from the same `IllegalStateException`
/// delivered later. That is a different defect from "the exception was lost",
/// and it is worse: the statements between the native's return and the
/// eventual delivery all executed.
///
/// Validate `JNI_OnLoad` completion (`Java_jdk_internal_loader_NativeLibraries_load`):
/// a pending JNI exception or an unsupported JNI version (including `JNI_ERR = -1`)
/// rolls back any partial `JNI_OnLoad` state and fails the library load so the
/// caller drops (`dlclose`s) the library handle instead of publishing it.
pub(in crate::vm::vm_exec) fn finish_jni_onload(
    shared: &SharedVm,
    thread: &mut JvmThread,
    version: crate::native::jni::JInt,
    resolved: &str,
    is_netty_tcnative: bool,
    jni_methods_before: std::collections::HashMap<u64, usize>,
) -> Result<bool, crate::error::MethodCallFailed> {
    let pending_exc = jni_pending_exception_after_native(shared, thread);
    let onload_ok =
        pending_exc.is_none() && crate::native::jni::is_supported_jni_version(version);
    if !onload_ok {
        if is_netty_tcnative {
            shared
                .natives
                .netty_tcnative_real
                .store(false, std::sync::atomic::Ordering::Release);
            shared
                .natives
                .native_methods
                .unmute_netty_tcnative_stubs();
        }
        let mut table = shared.natives.jni_native_methods.write();
        if *table != jni_methods_before {
            *table = jni_methods_before;
            drop(table);
            shared
                .natives
                .jni_native_generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        } else {
            drop(table);
        }
        if let Some(err) = pending_exc {
            return Err(err);
        }
        return Err(crate::error::RuntimeError::UnsatisfiedLinkError {
            message: format!(
                "unsupported JNI version 0x{:08X} required by {}",
                version as u32, resolved
            ),
        }
        .into());
    }
    Ok(is_netty_tcnative)
}

/// Mirrors `safe_native_call`'s handling, including the `u64::MAX` sentinel a
/// producer with no thread context uses, and the preference for
/// `native_pending_return` (a GC root remapped in place) over the raw handle.
pub(in crate::vm::vm_exec) fn jni_pending_exception_after_native(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Option<MethodCallFailed> {
    let exc_handle = crate::native::jni::take_jni_pending_exception()?;
    if exc_handle == u64::MAX {
        thread.native_pending_return = None;
        return Some(crate::runtime::exceptions::throw_runtime_error(
            shared,
            thread,
            RuntimeError::IllegalStateException {
                message: "JNI ThrowNew pending exception".to_string(),
            },
        ));
    }
    let ptr = exc_handle as *mut u8;
    if ptr.is_null() || (ptr as usize) % 8 != 0 {
        return None;
    }
    let exc_ref = thread
        .native_pending_return
        .unwrap_or_else(|| unsafe { crate::types::ObjectRef::from_raw(ptr) });
    thread.native_pending_return = Some(exc_ref);
    Some(MethodCallFailed::ExceptionThrown(exc_ref))
}

/// gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 1c): run `define` -- a class definition that touches `shared` only,
/// whose class-manager write guard posts a C agent's `ClassLoad` when it
/// drops -- with `thread`'s JNI binding installed from `thread` itself while a
/// C env listens for `ClassLoad` (`post_on_own_thread`), so the callback gets
/// this handle rather than the registry's address of the same thread. One load
/// otherwise. `define` must not reach `thread`.
#[cfg_attr(not(feature = "experimental-debug"), allow(unused_variables))]
pub(in crate::vm::vm_exec) fn define_on_own_handle<R>(shared: &SharedVm, thread: &mut JvmThread, define: impl FnOnce() -> R) -> R {
    #[cfg(feature = "experimental-debug")]
    if shared.debug.debugger_gates.native_class_load_armed() {
        return crate::runtime::interpreter::post_on_own_thread(thread, define);
    }
    define()
}

/// RAII guard for the JNI thread-local context (`*mut SharedVm` Arc + the
/// erased `*mut JvmThread` pointer) installed around a native bridge call.
///
/// `replace_jni_context` / `replace_jni_thread` publish pointers into thread-local
/// storage that the native bridge (and any JNI up-call it makes) dereferences;
/// `jni.rs` documents that the matching `clear_*` MUST run after every native
/// call returns *including on panic/unwind paths*. The previous code cleared
/// them with two manual statements after `dispatch_jni_native`, so a panic
/// unwinding out of the native bridge skipped the clears and left dangling
/// `*mut JvmThread` / `*mut SharedVm` pointers in TLS — a later JNI access on
/// the same OS thread would dereference freed memory (use-after-free).
///
/// Constructing the guard installs the context; its `Drop` runs the clears on
/// every exit edge — normal return, early `return Err(..)`, and unwind —
/// mirroring [`SynchronizedMethodGuard`]'s discipline. This is purely a
/// cleanup guard; it never re-installs context, so a double clear is harmless
/// (the `clear_*` setters are idempotent no-ops on already-cleared TLS).
pub(in crate::vm::vm_exec) struct JniContextGuard {
    /// What was installed before this call, put back on `Drop`.
    ///
    /// `None`/null at the outermost native call, which makes the restore a
    /// strict generalisation of the clear it replaces.
    /// gcf f4/j: `Kept` for a nested dispatch into the VM already installed.
    /// `Option` only so `Drop` can take it.
    pub(in crate::vm::vm_exec) prev_vm: Option<crate::native::jni::JniContextSave>,
    pub(in crate::vm::vm_exec) prev_thread: *mut JvmThread,
    /// `Some(previous value)` when this guard set `jni_native_class`
    /// (`install_for_native`); put back on `Drop`.
    pub(in crate::vm::vm_exec) prev_native_class: Option<Option<ClassId>>,
    pub(in crate::vm::vm_exec) thread: *mut JvmThread,
}

impl JniContextGuard {
    /// Install the JNI TLS context and return the guard that will put back
    /// whatever was there before.
    ///
    /// # Why SAVE/RESTORE and not set/clear
    ///
    /// A JNI native is not the only thing that can be on this stack. A native
    /// that calls back into Java, whose Java calls another JNI native, nests:
    ///
    /// ```text
    ///   install (outer) -> Java upcall -> install (inner) -> DROP (inner)
    /// ```
    ///
    /// and the inner `Drop` used to CLEAR. The outer native then continued to
    /// run with no context, and every upcall it made after its Java callback
    /// returned was answered as though the VM were not attached —
    /// `with_jni_context` returns `None`, `CallObjectMethodV` hands the host
    /// library a null `jobject`, and the library reads that as an answer.
    ///
    /// MEASURED (`probes/OpenSslTls13ReentryBisectProbe.java`): with netty's
    /// BoringSSL provider and `useTasks=false`, BoringSSL runs netty's verify
    /// callback INSIDE `SSL_do_handshake`. One nested tcnative call from that
    /// Java — any of them, including `SSL.getLastErrorNumber()`, which touches
    /// no `SSL*` — made BoringSSL's later CERTIFICATE callback reach no Java at
    /// all: the client's key manager was never asked for an alias (`keyAsks=0`
    /// against `1` on every passing row), so the client sent no certificate and
    /// the server ended the handshake with
    /// `PEER_DID_NOT_RETURN_A_CERTIFICATE`. The monitor-only, Java-only,
    /// allocation and `System.gc()` arms all passed, which is what rules out
    /// the lock-ordering, GC-relocation and BoringSSL-state hypotheses the page
    /// listed. See
    /// `nested-jni-call-cleared-the-enclosing-natives-context-FIXED-20260828.md`.
    ///
    /// The outermost call still drops the `Arc` on exit, because what it saved
    /// and restores is `None` — the release property `clear_jni_context`'s
    /// contract is about is preserved exactly.
    ///
    /// # Safety
    ///
    /// `thread` must remain a valid, exclusively-accessible `*mut JvmThread`
    /// for the lifetime of the returned guard (i.e. for the whole native
    /// call), exactly as required by [`crate::native::jni::set_jni_thread`].
    pub(in crate::vm::vm_exec) unsafe fn install(shared: &SharedVm, thread: *mut JvmThread) -> Self {
        let prev_vm = Some(crate::native::jni::enter_jni_context(shared));
        let prev_thread = crate::native::jni::replace_jni_thread(thread);
        // gci i12/ev (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): the thread's
        // own handle is installed now, before any C code of this call runs:
        // deliver the `ClassLoad`s deferred on it. One load while none waits.
        #[cfg(feature = "experimental-debug")]
        if !thread.is_null()
            && shared
                .debug
                .deferred_class_loads_pending
                .load(std::sync::atomic::Ordering::Acquire)
        {
            // `thread_id`: an immutable `Copy` field of the `JvmThread` this
            // function's contract makes valid.
            let tid = (*thread).thread_id.0;
            crate::jvmti::native_env::flush_deferred_class_loads(shared, tid);
        }
        JniContextGuard {
            prev_vm,
            prev_thread,
            prev_native_class: None,
            thread,
        }
    }

    /// [`Self::install`] for a JNI native dispatch: also records the native's
    /// declaring class on the thread for JNI `FindClass`
    /// (`r12w8-compat2-jni-findclass-native-declaring-class-patch`).
    ///
    /// # Safety
    ///
    /// As [`Self::install`]; `thread` must be non-null.
    pub(in crate::vm::vm_exec) unsafe fn install_for_native(
        shared: &SharedVm,
        thread: *mut JvmThread,
        declaring: ClassId,
    ) -> Self {
        let mut guard = Self::install(shared, thread);
        guard.prev_native_class = Some((*thread).jni_native_class.replace(declaring));
        guard
    }
}

impl Drop for JniContextGuard {
    fn drop(&mut self) {
        if let Some(prev) = self.prev_native_class.take() {
            // Safety: `thread` outlives the guard (`install`'s contract).
            unsafe { (*self.thread).jni_native_class = prev };
        }
        if let Some(save) = self.prev_vm.take() {
            crate::native::jni::leave_jni_context(save);
        }
        // Safety: `prev_thread` is whatever this thread had installed when the
        // guard was created, and an enclosing guard keeps it valid for at least
        // as long as this one lives.
        unsafe { crate::native::jni::restore_jni_thread(self.prev_thread) };
    }
}

/// GC-correctness (vm-jni-roots #2): RAII guard that brackets a JNI native
/// dispatch with an IMPLICIT local-reference frame.
///
/// A native that obtains a fresh local jobject (NewObject, GetObjectField, …)
/// expects it to stay live until the native returns. Those handles are tracked
/// via `track_local_ref`, but without an enclosing frame they had no scope and
/// were dropped from the root set. JNI semantics say every native call runs
/// inside an implicit local frame whose refs are freed on return; this guard
/// supplies it: `push_local_frame` on construction, `truncate_local_frames`
/// back to the entry depth on `Drop` (gc-common w7-c; it also closes any
/// frame the native pushed and leaked). The frame is therefore released on
/// the normal-return path, on the arity-mismatch early return, AND on a panic
/// unwind through the unsafe dispatch. While it lives, every local ref the
/// native creates is a GC root (scanned by `collect_local_ref_roots`) and is
/// remapped by `update_local_refs_after_gc` if a moving collection relocates
/// it.
pub(in crate::vm::vm_exec) struct JniImplicitFrameGuard(pub(in crate::vm::vm_exec) usize);

impl JniImplicitFrameGuard {
    #[inline]
    pub(in crate::vm::vm_exec) fn enter() -> Self {
        // gc-common w7-c: remember the depth, so the exit closes every frame
        // opened inside this call, a leaked `PushLocalFrame` included (HotSpot's
        // native wrapper restores the thread's handle block on return).
        let base = crate::native::jni::local_frame_depth();
        crate::native::jni::push_local_frame(16);
        JniImplicitFrameGuard(base)
    }
}

impl Drop for JniImplicitFrameGuard {
    #[inline]
    fn drop(&mut self) {
        // Close the implicit frame and anything the native leaked above it,
        // discarding their handles. We promote nothing: the dispatch result is
        // a `Value`, not a JObject borrowed from this frame, so there is no
        // local ref that must outlive the call here.
        crate::native::jni::truncate_local_frames(self.0);
    }
}


/// The hardware `int`/`long` arm of [`compare_and_swap_field_shared`]:
/// `Some(swapped)` when `fwd`'s slot `index` is a declared field of exactly
/// that width that `hw_atomic_addr` can address, `None` for anything the lock
/// path has to serve (arrays, a reference or mismatched pair, an undeclared
/// index, a LEGACY cell whose tag is not already the right type).
///
/// `fwd` must already be forwarded. One function for both callers --
/// `compare_and_swap_field_shared` and the JIT's
/// `Unsafe.compareAndSet{Int,Long}` thin helper -- so a compiled CAS and an
/// interpreted one agree on which slots are hardware-atomic and address the
/// same word; the atomicity-domain split `atomic_fetch_add_int` documents is
/// exactly what two copies of this decision would reintroduce.
///
/// No barrier on either arm: the payload is primitive, so no reference is
/// overwritten and none escapes.
pub(crate) fn hw_cas_primitive_field(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    match (expected, new_val) {
        (Value::Int(exp), Value::Int(new)) => {
            let Some(addr) = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Int) else {
                // JDK 25 `Unsafe.compareAndSetFloat` is Java calling
                // `compareAndSetInt` with the raw bits: an `int` CAS on a
                // `float` field. And (wave 4c) the `Z`/`B`/`C`/`S` fields,
                // whose CAS reaches here as an `int` (`compareAndSetShort`,
                // the exchange natives' CAS step).
                return hw_cas_float_bits(shared, fwd, index, exp as u32, new as u32)
                    .or_else(|| hw_cas_subword(shared, fwd, index, exp, new));
            };
            // SAFETY: see `atomic_fetch_add_int`.
            let cell = unsafe { hw_atomic_i32(addr) };
            Some(
                cell.compare_exchange(
                    exp,
                    new,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok(),
            )
        }
        (Value::Long(exp), Value::Long(new)) => {
            let Some(addr) = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Long) else {
                // `Unsafe.compareAndSetDouble`: a `long` CAS of raw bits on a
                // `double` field.
                return hw_cas_double_bits(shared, fwd, index, exp as u64, new as u64);
            };
            // SAFETY: see `atomic_fetch_add_int`.
            let cell = unsafe { hw_atomic_i64(addr) };
            Some(
                cell.compare_exchange(
                    exp,
                    new,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok(),
            )
        }
        // The `VarHandle` fast arms and the interpreter's CAS hand floating
        // operands as such.
        (Value::Float(exp), Value::Float(new)) => {
            hw_cas_float_bits(shared, fwd, index, exp.to_bits(), new.to_bits())
        }
        (Value::Double(exp), Value::Double(new)) => {
            hw_cas_double_bits(shared, fwd, index, exp.to_bits(), new.to_bits())
        }
        _ => None,
    }
}

/// JIT round 22 wave 4b, lane sync (`CRATONVM_HW_FLOAT_CAS`, HotSpot parity,
/// default ON, `=0` restores the lock path; page
/// `docs/feature-designs/real-jdk-mode.md`):
/// a CAS of a `float` field as ONE `LOCK CMPXCHG` of its raw 32 bits, as
/// HotSpot's `compareAndSetFloat` (`compareAndSetInt(floatToRawIntBits)`) is.
/// The lock path it replaces read, compared and wrote under the CAS and
/// stripe locks, which a compiled volatile store (an inline store and a fence)
/// never takes, so a store between the read and the write was lost and the CAS
/// still reported success. Raw bits: `NaN` patterns compare by pattern and
/// `-0.0` differs from `0.0`, exactly `values_equal_for_cas`'s answer. `None`
/// (the lock path) for a field `hw_atomic_addr` refuses: not `F`, a legacy cell
/// not yet tagged `Float` (an uninitialized cell keeps the lock path's
/// typed-default coercion), a misaligned payload.
pub(in crate::vm::vm_exec) fn hw_cas_float_bits(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    exp: u32,
    new: u32,
) -> Option<bool> {
    if !shared.hw_float_cas {
        return None;
    }
    let addr = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Float)?;
    // SAFETY: `addr` is a naturally aligned 4-byte payload of a live,
    // forwarded object (`hw_atomic_addr`), as for `hw_atomic_i32`.
    let cell = unsafe { &*(addr as *const std::sync::atomic::AtomicU32) };
    Some(
        cell.compare_exchange(
            exp,
            new,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok(),
    )
}

/// [`hw_cas_float_bits`] for a `double` field: one `LOCK CMPXCHG` of its raw
/// 64 bits (HotSpot's `compareAndSetDouble`).
pub(in crate::vm::vm_exec) fn hw_cas_double_bits(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    exp: u64,
    new: u64,
) -> Option<bool> {
    if !shared.hw_float_cas {
        return None;
    }
    let addr = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Double)?;
    // SAFETY: as in `hw_cas_float_bits`, an aligned 8-byte payload.
    let cell = unsafe { &*(addr as *const std::sync::atomic::AtomicU64) };
    Some(
        cell.compare_exchange(
            exp,
            new,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok(),
    )
}

/// The `Value` tag and payload bits of `v` as a static cell stores them
/// (`Value` is `#[repr(u32)]`: tag at 0, a 4-byte payload at
/// `FIELD_CELL_PAYLOAD32_OFFSET`, an 8-byte one at `..PAYLOAD64_OFFSET`), or
/// `None` for a value no hardware static cell holds.
pub(in crate::vm::vm_exec) fn static_cell_bits(v: Value) -> Option<(u32, u64)> {
    Some(match v {
        Value::Int(i) => (0, u64::from(i as u32)),
        Value::Long(l) => (1, l as u64),
        Value::Float(f) => (2, u64::from(f.to_bits())),
        Value::Double(d) => (3, d.to_bits()),
        Value::Object(None) => (4, 0),
        Value::Object(Some(r)) => (4, r.as_ptr() as usize as u64),
        _ => return None,
    })
}

/// The `Value` a static cell with `tag` and payload `bits` holds (the inverse
/// of [`static_cell_bits`]).
pub(in crate::vm::vm_exec) fn static_cell_value(tag: u32, bits: u64) -> Option<Value> {
    Some(match tag {
        0 => Value::Int(bits as u32 as i32),
        1 => Value::Long(bits as i64),
        2 => Value::Float(f32::from_bits(bits as u32)),
        3 => Value::Double(f64::from_bits(bits)),
        4 if bits == 0 => Value::Object(None),
        // SAFETY: a non-zero reference payload of a static cell is a live
        // object (statics are GC roots, remapped in place).
        4 => Value::Object(Some(unsafe { ObjectRef::from_raw(bits as usize as *mut u8) })),
        _ => return None,
    })
}

/// JIT round 22 wave 5b, lane sync (`CRATONVM_HW_STATIC_CAS`, HotSpot parity,
/// default ON, `=0` restores the mirror-monitor lock path; review
/// `r22-review-w5-inline-on-sync-FIXED-20261020.md` item 7): the hardware
/// read-modify-write behind `NativeSystemAccess::static_field_hw_update`.
///
/// A static slot is a 16-byte `Value` cell of a LEAKED `StaticsBlock`
/// (`StaticsIndex::cell_ptr`): it never moves, so no GC re-resolution is
/// needed during the CAS; the cell is re-resolved on every retry anyway, so a
/// republication (a grown block, a re-prepare) is followed exactly as
/// `set_static_shared`'s lock-free store follows it. Each attempt reads the
/// tag, then the payload (4 bytes at `+4` for `int` / sub-word / `float`, 8 at
/// `+8` for `long` / `double` / reference), re-checks the tag, decodes the
/// `Value` `get_static_field` would answer, asks `f`, and stores the new payload
/// with one `LOCK CMPXCHG` -- atomic against a compiled or interpreted
/// `putstatic`, which writes the cell in place. Declines (`None`, nothing
/// written): the switch off, `CRATONVM_NO_STATICS_INDEX`, an unpublished slot,
/// a tag outside the five kinds, a new value whose tag is not the cell's (the
/// payload-only store would leave the tag lying), a `float` / `double` cell
/// with `CRATONVM_HW_FLOAT_CAS=0`. Static reference payloads hold raw
/// pointers even when ZGC colors heap slots. Use the same atomic word as
/// `compare_and_exchange_static_shared` so the two operations remain atomic
/// against each other. A reference store fires the SATB pre-barrier on the
/// overwritten value first, as `set_static_shared` does; statics are roots, so
/// no card is owed.
pub(crate) fn hw_static_cell_update(
    shared: &SharedVm,
    class_id: ClassId,
    field_index: usize,
    f: &mut dyn FnMut(Value) -> Option<Value>,
) -> Option<(Value, bool)> {
    use std::sync::atomic::Ordering::SeqCst;
    if !shared.hw_static_cas || super::vm_object::statics_index_disabled() {
        return None;
    }
    loop {
        let cell = shared.classes.statics_index.cell_ptr(class_id, field_index)? as usize;
        if cell % 8 != 0 {
            return None;
        }
        // SAFETY: `cell_ptr` bounds-checked the slot of a leaked block; the
        // cell is 8-aligned and 16 bytes long.
        let tag_word = unsafe { &*(cell as *const std::sync::atomic::AtomicU32) };
        let tag = tag_word.load(SeqCst);
        let wide = match tag {
            0 => false,
            2 if shared.hw_float_cas => false,
            1 | 4 => true,
            3 if shared.hw_float_cas => true,
            _ => return None,
        };
        let bits = if wide {
            // SAFETY: the aligned 8-byte payload word of the cell above.
            unsafe {
                &*((cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET)
                    as *const std::sync::atomic::AtomicU64)
            }
            .load(SeqCst)
        } else {
            // SAFETY: the aligned 4-byte payload of the cell above.
            u64::from(
                unsafe {
                    &*((cell + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET)
                        as *const std::sync::atomic::AtomicU32)
                }
                .load(SeqCst),
            )
        };
        if tag_word.load(SeqCst) != tag {
            // A store re-tagged the cell between the two reads: read again.
            continue;
        }
        let current = static_cell_value(tag, bits)?;
        let Some(new) = f(current) else {
            return Some((current, false));
        };
        let (new_tag, new_bits) = static_cell_bits(new)?;
        if new_tag != tag {
            return None;
        }
        if let Value::Object(Some(_)) = current {
            shared.mem.heap.satb_barrier(current);
        }
        let swapped = if wide {
            // SAFETY: as for the load above.
            unsafe { &*((cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *const std::sync::atomic::AtomicU64) }
                .compare_exchange(bits, new_bits, SeqCst, SeqCst)
                .is_ok()
        } else {
            // SAFETY: as for the load above.
            unsafe {
                &*((cell + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET)
                    as *const std::sync::atomic::AtomicU32)
            }
            .compare_exchange(bits as u32, new_bits as u32, SeqCst, SeqCst)
            .is_ok()
        };
        if swapped {
            return Some((current, true));
        }
    }
}

/// The cell a hardware sub-word CAS addresses ([`hw_subword_cell`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_exec) enum SubwordCell {
    /// A compact `boolean` / `byte` field: one byte at the field address.
    Compact8,
    /// A compact `char` / `short` field: two bytes, 2-aligned.
    Compact16,
    /// A legacy 16-byte `Value` cell already tagged `Int`: its 4-byte payload.
    Legacy32,
}

/// JIT round 22 wave 4c, lane sync: the address, descriptor and cell shape of
/// a `Z` / `B` / `C` / `S` instance field of the forwarded `obj`, for one
/// hardware CAS -- or `None` (the lock path) for anything not proven exact: an
/// array, another descriptor, a compact slot whose storage width or alignment
/// is not the descriptor's, a legacy cell not tagged `Int` (an uninitialized
/// cell keeps the lock path's typed-default coercion), a misaligned address.
///
/// Never publishes anything: unlike [`hw_atomic_addr_checked`] this lookup
/// has no path into `JitRealm::unsafe_inline`, so no compiled `Unsafe`
/// int / long / reference prefix can ever be handed a sub-word cell (pinned
/// by `r22_sync_hw_float_cas_tests`). Only [`hw_cas_subword`] publishes a
/// compact cell, into the table's own sub-word half (JIT round 21, UNSAFE21-2).
pub(in crate::vm::vm_exec) fn hw_subword_cell(
    shared: &SharedVm,
    obj: ObjectRef,
    index: usize,
) -> Option<(usize, u8, SubwordCell)> {
    if shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let class_id = shared.mem.heap.class_id_of(obj);
    let desc = resolve_field_descriptor_byte_cached(shared, class_id, index)?;
    let (want, compact_cell) = match desc {
        b'Z' | b'B' => (1usize, SubwordCell::Compact8),
        b'C' | b'S' => (2usize, SubwordCell::Compact16),
        _ => return None,
    };
    let header = shared.mem.heap.get_header(obj);
    let compact = header.gc_flags() & cratonvm_types::GC_FLAG_COMPACT != 0;
    let base = obj.as_ptr() as usize;
    if compact {
        let (disp, storage) = cratonvm_types::compact_field_disp_storage(class_id.as_u32(), index)?;
        if cratonvm_types::FieldStorageKind::from_descriptor_byte(desc) != Some(storage)
            || storage.size_runtime() as usize != want
        {
            return None;
        }
        let addr = base.checked_add(disp)?;
        if addr % want != 0 {
            return None;
        }
        Some((addr, desc, compact_cell))
    } else {
        if index >= header.num_slots() as usize {
            return None;
        }
        let cell = index
            .checked_mul(cratonvm_types::SLOT_SIZE)
            .and_then(|off| base.checked_add(cratonvm_types::HEADER_SIZE)?.checked_add(off))?;
        if cell % std::mem::align_of::<u32>() != 0 {
            return None;
        }
        // SAFETY: `cell` is the start of a live 16-byte `Value` slot of a
        // forwarded object (`index < num_slots`).
        let tag = unsafe {
            (*(cell as *const std::sync::atomic::AtomicU32))
                .load(std::sync::atomic::Ordering::Relaxed)
        };
        // `Value::Int`'s discriminant (`field_cell_layout_matches_value_enum`).
        if tag != 0 {
            return None;
        }
        let addr = cell.checked_add(cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET)?;
        if addr % 4 != 0 {
            return None;
        }
        Some((addr, desc, SubwordCell::Legacy32))
    }
}

/// The value a LEGACY `Z` / `B` / `C` / `S` cell holds for `v` as a byte /
/// short CAS sees it: `B` / `S` sign-extend 8 / 16 bits, `C` zero-extends 16
/// bits, and `Z` is the RAW byte, zero-extended (JIT round 22 wave 5c, lane
/// sync, review `r22-review-w5-memory-on-sync` item 1: HotSpot's
/// `compareAndSetByte` / `compareAndExchangeByte` on a `boolean` field is a
/// `LOCK CMPXCHG` of the raw byte; `putfield` masks `& 1` before it stores,
/// `Unsafe.putByte` does not).
pub(in crate::vm::vm_exec) fn narrow_subword(desc: u8, v: i32) -> i32 {
    match desc {
        b'Z' => i32::from(v as u8),
        b'B' => i32::from(v as i8),
        b'S' => i32::from(v as i16),
        b'C' => i32::from(v as u16),
        _ => v,
    }
}

/// JIT round 22 wave 4c, lane sync (`CRATONVM_HW_SUBWORD_CAS`, HotSpot parity,
/// default ON, `=0` restores the lock path; the sub-word half of
/// `r22-w4-float-double-field-cas-is-lock-based-not-atomic-against-compiled-stores-FIXED-20261020.md`):
/// a CAS of a `boolean` / `byte` / `char` / `short` field as ONE hardware
/// compare-exchange, atomic against compiled stores of the field, as HotSpot's
/// `LOCK CMPXCHG` (byte / word forms) is.
///
/// * Compact cell: `LOCK CMPXCHG` of the byte / word itself (`AtomicU8` /
///   `AtomicU16`), operands narrowed exactly as `write_compact_field` stores
///   them (`Z` and `B`: the raw low 8 bits, wave 5c; `C` / `S`: low 16 bits), so the
///   compare is the stored pattern's.
/// * Legacy cell: a compare-exchange loop on the 32-bit payload that compares
///   the NARROWED values (the field's own width, whatever extension the cell
///   happens to hold) and stores the update narrowed as `putfield` does; a
///   retry only when the payload changed under it.
pub(in crate::vm::vm_exec) fn hw_cas_subword(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    exp: i32,
    new: i32,
) -> Option<bool> {
    use std::sync::atomic::Ordering::SeqCst;
    if !shared.hw_subword_cas {
        return None;
    }
    // JIT round 21, lane atomics (UNSAFE21-2): read BEFORE the displacement
    // is resolved (the cell table's epoch rule).
    let epoch = shared.jit.unsafe_inline.subword_epoch_if_armed();
    let (addr, desc, cell) = hw_subword_cell(shared, fwd, index)?;
    if let (Some(epoch), SubwordCell::Compact8 | SubwordCell::Compact16) = (epoch, cell) {
        // The compact field is this one hardware `LOCK CMPXCHG` of its width
        // on every path, so the compiled sub-word CAS prefixes may do the same
        // on its bytes: publish `(class, slot, width) -> disp` into the
        // sub-word half only (never the int/long or reference cells).
        shared.jit.unsafe_inline.note_compact_subword_field(
            shared.mem.heap.class_id_of(fwd).as_u32(),
            index,
            cell == SubwordCell::Compact16,
            addr - fwd.as_ptr() as usize,
            epoch,
        );
    }
    match cell {
        SubwordCell::Compact8 => {
            // `Z` like `B`: the raw byte (wave 5c; see `narrow_subword`).
            let _ = desc;
            let (e, n) = (exp as u8, new as u8);
            // SAFETY: a live, forwarded object's 1-byte field (`hw_subword_cell`).
            let cell = unsafe { &*(addr as *const std::sync::atomic::AtomicU8) };
            Some(cell.compare_exchange(e, n, SeqCst, SeqCst).is_ok())
        }
        SubwordCell::Compact16 => {
            // SAFETY: a live, forwarded object's 2-aligned 2-byte field.
            let cell = unsafe { &*(addr as *const std::sync::atomic::AtomicU16) };
            Some(cell.compare_exchange(exp as u16, new as u16, SeqCst, SeqCst).is_ok())
        }
        SubwordCell::Legacy32 => {
            // SAFETY: a live, forwarded object's aligned 4-byte `Int` payload.
            let cell = unsafe { &*(addr as *const std::sync::atomic::AtomicI32) };
            let want = narrow_subword(desc, exp);
            let update = narrow_subword(desc, new);
            let mut cur = cell.load(SeqCst);
            loop {
                if narrow_subword(desc, cur) != want {
                    return Some(false);
                }
                match cell.compare_exchange(cur, update, SeqCst, SeqCst) {
                    Ok(_) => return Some(true),
                    Err(actual) => cur = actual,
                }
            }
        }
    }
}

/// JIT round 21 wave 1, lane updaters
/// (`CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL`, opt-in): publish the
/// `Unsafe` field cell of `fwd`'s `int` (`wide == false`) / `long` slot
/// `index` from the volatile get / put thin helpers, which reach the field by
/// `get_field_volatile` / `set_field_volatile` and so never through
/// [`hw_atomic_addr`], the only publisher. Without this a field that is only
/// read or written through `Unsafe` (an updater's `get` / `set` loop, a
/// `LongAdder` base read) never gets the compiled get / put arm.
///
/// Sound by the same rule as every other publication: `hw_atomic_addr`
/// publishes only after its own guards (declared `I` / `J`, compact, exact
/// width, aligned), i.e. for exactly the fields whose volatile access is ONE
/// aligned atomic load / store on every collector
/// (`cratonvm_jit::direct_helpers::unsafe_volatile_field_arm_words`'s audit).
/// Nothing is written here; the address is discarded. One relaxed load when
/// the switch is off (`UnsafeInlineCells::volatile_publish_armed`).
///
/// `kind` is `b'I'`, `b'J'` or (UPDATERS21-1) `b'L'`: a reference cell is
/// published into the reference half only where `hw_atomic_addr` publishes one
/// (Generational / G1, armed by a compile that bakes a reference row), only
/// when a compile also armed the reference get / put arm
/// (`UnsafeInlineCells::ref_volatile_publish_armed`), and never for
/// `java.lang.ref.Reference`'s own fields (slots 0..4 of a subclass instance),
/// whose `referent` a compiled plain load must not read (the natives keep it
/// alive). A field whose cell is published, owned by a colliding field, or
/// unfit returns after one table load (`field_cell_settled`, under the
/// table's current epoch only); a failed attempt records a refusal for that
/// key (`note_field_refusal`), not for the VM -- since r21 wave 6 (lane
/// updaters, review item 6) only a refusal fixed for the class's lifetime or
/// its layout epoch (`hw_atomic_refusal_settles`); a legacy instance records
/// nothing and a transient refusal spends one backstop attempt instead.
pub(crate) fn note_unsafe_volatile_field_cell(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    kind: u8,
) {
    crate::runtime::frozen_pages::note_slot_write(shared, fwd, index, true);
    let cells = &shared.jit.unsafe_inline;
    if !cells.volatile_publish_armed() {
        return;
    }
    // A/B fix (`R21UpdaterRows` on cvm-r21w1): the settled test comes first
    // and costs one table load, so a published (or unpublishable) field pays
    // no lookup per call; before, every reference get / put thin call walked
    // the class chain under the class-manager lock and re-ran
    // `hw_atomic_addr`, for a cell no arm read.
    let class_id = shared.mem.heap.class_id_of(fwd);
    let (kind, wide, reference) = match kind {
        b'I' => (HwAtomicKind::Int, false, false),
        b'J' => (HwAtomicKind::Long, true, false),
        b'L' => {
            if !cells.ref_volatile_publish_armed() || cells.ref_epoch_if_armed().is_none() {
                return;
            }
            (HwAtomicKind::Reference, true, true)
        }
        _ => return,
    };
    if cells.field_cell_settled(class_id.as_u32(), index, wide, reference) {
        return;
    }
    // JIT round 21 wave 6, lane updaters (review item 6): only the COMPACT
    // arm of `hw_atomic_addr` publishes, and a class with a compact layout
    // may still have LEGACY instances (the arm is chosen per object). A
    // legacy instance therefore says nothing about the key: before, its
    // refusal (or even its successful legacy-cell answer) settled the key,
    // and the field's later compact instances never got the get / put arm.
    // Now it returns here, recording nothing, spending nothing, and paying
    // no lookup.
    if shared.mem.heap.get_header(fwd).gc_flags() & cratonvm_types::GC_FLAG_COMPACT == 0 {
        return;
    }
    // The layout epoch the attempt begins in: a refusal is recorded under it
    // only, and only if it is still live (`note_field_refusal`).
    let epoch = cratonvm_types::layout_replace_epoch();
    // `java.lang.ref.Reference`'s own four fields (`referent`, `queue`,
    // `next`, `discovered`) are slots 0..4 of every subclass: the class walk
    // is asked of those slots only. A superclass chain is the class's
    // lifetime, so the refusal settles the key.
    if reference && index < 4 && class_is_java_lang_ref_reference(shared, class_id) {
        cells.note_field_refusal(class_id.as_u32(), index, wide, reference, epoch);
        return;
    }
    let settles = match hw_atomic_addr_checked(shared, fwd, index, kind) {
        // Every guard passed on a compact instance and nothing was
        // published: the cell word cannot name the displacement (past
        // `u16`), or this half has no publisher on this heap (a reference
        // off Generational / G1) -- both fixed for the layout epoch -- or
        // the epoch moved under the attempt (`publish_slow` refused), which
        // the epoch test below catches.
        Ok(_) => true,
        Err(why) => hw_atomic_refusal_settles(shared, class_id, index, why),
    };
    // Lane fuzz2's page, half 2: a refusal is recorded for this key only.
    if cells.field_cell_settled(class_id.as_u32(), index, wide, reference) {
        return;
    }
    if settles && cratonvm_types::layout_replace_epoch() == epoch {
        cells.note_field_refusal(class_id.as_u32(), index, wide, reference, epoch);
    } else {
        // Transient (a per-object refusal, an unconfirmed descriptor miss,
        // an epoch that moved): the key stays open; the bounded backstop
        // pays for the retry.
        cells.spend_publish_attempt();
    }
}

/// Is `class_id` `java.lang.ref.Reference` or a subclass of it? `true` (the
/// refusing answer) for a class the store does not know.
pub(crate) fn class_is_java_lang_ref_reference(
    shared: &SharedVm,
    class_id: cratonvm_types::ClassId,
) -> bool {
    let cm = shared.classes.class_manager.read();
    let store = cm.class_store();
    let mut walk = Some(class_id);
    while let Some(cid) = walk {
        let Some(class) = store.get(cid) else {
            return true;
        };
        if &*class.name == "java/lang/ref/Reference" {
            return true;
        }
        walk = class.superclass;
    }
    false
}

/// [`hw_cas_primitive_field`] returning the WITNESS (the value the cell held;
/// `expected` exactly when the exchange happened): `VarHandle.compareAndExchange`
/// for the interpreter's route-cache arm (interpreter round i2 wave 54, lane
/// L7, `jit::helpers::varhandle_instance_field_cae_values`). The same cells:
/// `None` for anything `hw_cas_primitive_field` would also decline, before
/// anything is written.
pub(crate) fn hw_cae_primitive_field(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<Value> {
    match (expected, new_val) {
        (Value::Int(exp), Value::Int(new)) => {
            let addr = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Int)?;
            // SAFETY: see `atomic_fetch_add_int`.
            let cell = unsafe { hw_atomic_i32(addr) };
            let witness = match cell.compare_exchange(
                exp,
                new,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(v) | Err(v) => v,
            };
            Some(Value::Int(witness))
        }
        (Value::Long(exp), Value::Long(new)) => {
            let addr = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Long)?;
            // SAFETY: see `atomic_fetch_add_int`.
            let cell = unsafe { hw_atomic_i64(addr) };
            let witness = match cell.compare_exchange(
                exp,
                new,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(v) | Err(v) => v,
            };
            Some(Value::Long(witness))
        }
        _ => None,
    }
}

/// The element encoding of one hardware primitive-array CAS
/// ([`hw_cas_primitive_array_element`]): the element's byte width, the
/// expected and new bit patterns as the payload holds them, and whether
/// `expected` is a value the element can hold at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HwArrayCasBits {
    pub(crate) width: usize,
    pub(crate) expected: u64,
    pub(crate) new: u64,
    /// `false` when no stored element decodes to `expected` (a `byte[]` CAS
    /// expecting 300, say): the CAS can only fail, as the lock path's
    /// decoded compare does. The witness is still read atomically.
    pub(crate) expected_representable: bool,
}

/// nio21 wave 5 L3: the bits of a CAS of `expected -> new_val` on an element
/// of type `et`, or `None` for a pair the hardware arm does not serve (a
/// reference element, a mismatched `Value` tag pair, the lock path's
/// `Object(None)`-as-zero coercion). The encodings are exactly
/// `gc::heap::write_prim_element`'s for the new value (a `boolean` stores
/// `v & 1`, a `byte` its low 8 bits, ..) and `read_prim_element`'s inverse
/// for the expected one (a `byte` reads sign-extended, a `boolean` and a
/// `char` zero-extended), so an element compares equal here exactly when
/// `values_equal_for_cas(get_array_element(..), expected)` would.
pub(crate) fn hw_array_cas_bits(
    et: cratonvm_types::ArrayElementType,
    expected: Value,
    new_val: Value,
) -> Option<HwArrayCasBits> {
    use cratonvm_types::ArrayElementType as E;
    // `canonical` is `e` read back through the element's decode: equal only
    // when some stored element reads as `e`.
    let narrow = |width: usize, e: i32, canonical: i32, stored: u64| HwArrayCasBits {
        width,
        expected: u64::from(e as u32) & ((1u64 << (width * 8)) - 1),
        new: stored,
        expected_representable: e == canonical,
    };
    Some(match (et, expected, new_val) {
        (E::Int, Value::Int(e), Value::Int(n)) => HwArrayCasBits {
            width: 4,
            expected: u64::from(e as u32),
            new: u64::from(n as u32),
            expected_representable: true,
        },
        (E::Float, Value::Float(e), Value::Float(n)) => HwArrayCasBits {
            width: 4,
            expected: u64::from(e.to_bits()),
            new: u64::from(n.to_bits()),
            expected_representable: true,
        },
        (E::Long, Value::Long(e), Value::Long(n)) => HwArrayCasBits {
            width: 8,
            expected: e as u64,
            new: n as u64,
            expected_representable: true,
        },
        (E::Double, Value::Double(e), Value::Double(n)) => HwArrayCasBits {
            width: 8,
            expected: e.to_bits(),
            new: n.to_bits(),
            expected_representable: true,
        },
        (E::Byte, Value::Int(e), Value::Int(n)) => {
            narrow(1, e, i32::from(e as i8), u64::from(n as u8))
        }
        (E::Boolean, Value::Int(e), Value::Int(n)) => {
            narrow(1, e, i32::from(e as u8), u64::from((n & 1) as u8))
        }
        (E::Char, Value::Int(e), Value::Int(n)) => {
            narrow(2, e, i32::from(e as u16), u64::from(n as u16))
        }
        (E::Short, Value::Int(e), Value::Int(n)) => {
            narrow(2, e, i32::from(e as i16), u64::from(n as i16 as u16))
        }
        _ => return None,
    })
}

/// The `Value` an element of type `et` holding `bits` reads as
/// (`gc::heap::read_prim_element`'s decode).
pub(crate) fn hw_array_decode(et: cratonvm_types::ArrayElementType, bits: u64) -> Value {
    use cratonvm_types::ArrayElementType as E;
    match et {
        E::Int => Value::Int(bits as u32 as i32),
        E::Float => Value::Float(f32::from_bits(bits as u32)),
        E::Long => Value::Long(bits as i64),
        E::Double => Value::Double(f64::from_bits(bits)),
        E::Byte => Value::Int(i32::from(bits as u8 as i8)),
        E::Boolean => Value::Int(i32::from(bits as u8)),
        E::Char => Value::Int(i32::from(bits as u16)),
        E::Short => Value::Int(i32::from(bits as u16 as i16)),
        E::Reference => Value::Object(None),
    }
}

/// nio21 wave 5 L3 (`docs/feature-designs/real-jdk-mode.md`):
/// a compare-and-swap on element `index` of the PRIMITIVE array `fwd` as ONE
/// hardware atomic on the element's payload address (`LOCK CMPXCHG` of the
/// element's width), answering `Some((witness, swapped))`.
///
/// Before this, an aligned width-matched `Unsafe.compareAndSet{Int,Long}` on
/// an `int[]` / `long[]` (`AtomicIntegerArray`, `AtomicLongArray`, an array
/// `VarHandle`) reached [`compare_and_swap_field_shared`], whose
/// [`hw_atomic_addr`] refuses arrays, and ran as a read, a compare and a write
/// under `monitors.with_cas_lock`. Plain element stores (`iastore`, the JIT's
/// `XCHG` volatile put, `set_array_element`) take no lock, so one could land
/// between that read and write and be overwritten while the CAS reported
/// success: a lost update HotSpot's `LOCK CMPXCHG` cannot produce, and an
/// atomicity domain no compiled `LOCK CMPXCHG` / `LOCK XADD` could join.
///
/// The payload is flat for every array on every collector
/// (`VmHeap::array_data_ptr`), every element is naturally aligned (the payload
/// starts 8-aligned at `ARRAY_DATA_OFFSET`), and a native reaches no safepoint
/// between the forward and the atomic, so the address is stable. A narrow
/// element (`byte`/`boolean`/`char`/`short`) is a 1- or 2-byte `LOCK CMPXCHG`,
/// which x86-64 and AArch64 (LSE or LL/SC) both provide.
///
/// `None` -- the caller keeps its lock path -- for anything that is not a
/// primitive array, an index out of range, a `Value` pair
/// [`hw_array_cas_bits`] does not serve, and under
/// `CRATONVM_UNSAFE_ARRAY_HW_CAS=0`. No barrier: the payload is primitive.
pub(crate) fn hw_cas_primitive_array_element(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<(Value, bool)> {
    use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering::SeqCst};
    if !crate::runtime::env_cache::unsafe_array_hw_cas_enabled() {
        return None;
    }
    let heap = &shared.mem.heap;
    if heap.kind_of(fwd) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    let et = heap.element_type_of_validated(fwd);
    if et == cratonvm_types::ArrayElementType::Reference || index >= heap.array_length(fwd) {
        return None;
    }
    let bits = hw_array_cas_bits(et, expected, new_val)?;
    if bits.width != cratonvm_types::element_byte_size(et) {
        return None;
    }
    let base = heap.array_data_ptr(fwd)?;
    let byte_off = index.checked_mul(bits.width)?;
    // SAFETY: `index < length`, so `byte_off + width` is inside the payload.
    let addr = unsafe { base.add(byte_off) };
    if (addr as usize) % bits.width != 0 {
        return None;
    }
    // SAFETY: `addr` is naturally aligned for `width` and points at `width`
    // live payload bytes of a forwarded array that cannot move before this
    // single atomic returns (see the function doc). Java code reaches these
    // bytes only through plain or atomic machine accesses of the same width.
    let (witness, swapped) = unsafe {
        macro_rules! cas {
            ($at:ty, $t:ty) => {{
                let a = <$at>::from_ptr(addr.cast::<$t>());
                if bits.expected_representable {
                    match a.compare_exchange(bits.expected as $t, bits.new as $t, SeqCst, SeqCst) {
                        Ok(w) => (u64::from(w), true),
                        Err(w) => (u64::from(w), false),
                    }
                } else {
                    (u64::from(a.load(SeqCst)), false)
                }
            }};
        }
        match bits.width {
            1 => cas!(AtomicU8, u8),
            2 => cas!(AtomicU16, u16),
            4 => cas!(AtomicU32, u32),
            8 => cas!(AtomicU64, u64),
            _ => return None,
        }
    };
    Some((hw_array_decode(et, witness), swapped))
}

/// The post barrier of a SUCCESSFUL reference CAS that stored `new_val` into
/// slot `index` of `fwd`: the ELEMENT's card for a reference array on the
/// generational heap (element-precise marks, as `jit_aastore_body` and
/// `set_array_element` do), the holder's barrier everywhere else. Shared by
/// the instance-field reference arm and [`hw_cas_reference_array_element`] so
/// the two hardware reference CASes cannot drift on which card they dirty.
pub(in crate::vm::vm_exec) fn ref_cas_post_barrier(shared: &SharedVm, fwd: ObjectRef, index: usize, new_val: Value) {
    match &shared.mem.heap {
        _ if shared.mem.heap.is_generational()
            && shared.mem.heap.kind_of(fwd) == cratonvm_types::ObjectKind::Array =>
        {
            shared.mem.heap.gen_write_barrier_ref_array_element(fwd, index, new_val)
        }
        _ => shared.mem.heap.write_barrier(fwd, new_val),
    }
}

/// nio21 wave 6 L3 (`r21-reference-array-unsafe-cas-is-lock-based-FIXED-20261008.md`):
/// a compare-and-swap on element `index` of the REFERENCE array `fwd` as ONE
/// hardware `LOCK CMPXCHG` on the element slot, answering
/// `Some((witness_raw_address, swapped))` (`0` is a JVM null).
///
/// The lock path this replaces read, compared and wrote the element under
/// `monitors.with_cas_lock`, which a plain `aastore`, a `putReferenceRelease`
/// / `putReferenceVolatile` or a `set_array_element` never takes: one of those
/// could land between the read and the write and be overwritten while the CAS
/// reported success (`ForkJoinPool$WorkQueue`'s slot CAS against the owner's
/// release store, `ConcurrentHashMap.casTabAt` against `setTabAt`,
/// `AtomicReferenceArray`). HotSpot's `LOCK CMPXCHG` cannot lose that store.
///
/// The slot is the heap's own: `array_data_ptr + index * ref_element_size()`,
/// a 4-byte encoded narrow oop under compressed oops (generational only;
/// `vm_init` refuses narrow oops on G1 and ZGC) and the 8-byte raw pointer
/// otherwise, exactly what `read_ref_slot` / `write_ref_slot` and the JIT's
/// `aastore` read and write. Element slots are naturally aligned
/// (`ARRAY_DATA_OFFSET` is 8-aligned).
///
/// Barriers are the instance-field reference arm's, in its order: the SATB
/// pre-barrier on `expected` BEFORE the store (on success that IS the
/// overwritten value; on failure the extra log only over-approximates the
/// live set, which SATB allows), the element card / holder barrier
/// ([`ref_cas_post_barrier`]) only on success.
///
/// `None` -- the caller keeps its lock path -- for anything that is not a
/// reference array, an index out of range, a non-reference `Value` pair (the
/// generic-`Value`-store autobox coercion lives on the lock path), a ZGC heap
/// whose colored-pointer load barrier is ARMED (no treatment of a colored CAS
/// witness is written yet; an unarmed ZGC heap is served since nio21 wave 7
/// L3), an unencodable narrow operand, and under
/// `CRATONVM_UNSAFE_REF_ARRAY_HW_CAS=0`.
pub(crate) fn hw_cas_reference_array_element(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<(u64, bool)> {
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    let (Value::Object(exp), Value::Object(new)) = (expected, new_val) else {
        return None;
    };
    if !crate::runtime::env_cache::unsafe_ref_array_hw_cas_enabled() {
        return None;
    }
    let heap = &shared.mem.heap;
    // Generational and G1; and ZGC while its colored-pointer load barrier is
    // NOT armed (nio21 wave 7 L3,
    // `r21-zgc-reference-array-unsafe-cas-stays-lock-based-FIXED-20261008.md`).
    // Unarmed, a ZGC slot holds the plain 8-byte pointer
    // (`VmHeap::load_ref_slot_barriered` is then literally `read_ref_slot`;
    // narrow oops are refused on ZGC at init), so the bit-level CAS below is
    // the G1 arm's, with the same barriers the instance-field reference arm
    // already applies on ZGC. Arming happens only at a safepoint, which this
    // native cannot be inside of, so the answer holds until the CAS returns.
    // Armed, a slot may hold a colored word that an uncolored `expected`
    // would fail against spuriously: that heals-then-CASes treatment is still
    // unwritten, so the lock path keeps serving it. The process-wide flag
    // over-approximates "this heap is armed" (`zgc_read_barrier_armed`'s own
    // doc), which errs toward the lock path.
    //
    // nio21 wave 9 L3 (`r21-armed-zgc-reference-cas-needs-heal-then-cas-CLOSED-20261012.md`):
    // `CRATONVM_ZGC_ARMED_REF_CAS` serves the armed heap too, as
    // heal-then-CAS (`VmHeap::cas_ref_slot_barriered`) instead of the bit CAS
    // below. It assumes every writer of an armed ELEMENT slot stores a
    // coloured word; JIT round 23 wave 2, lane L4 made that true
    // (`ZgcRealHeap::set_array_element` stores `armed_store_word`, the JIT
    // `aastore` helper routes there while armed, the interpreter's and the
    // JIT's inline `aastore` decline while armed) and flipped it default ON
    // (`=0` restores the lock path). Only the opt-in `CRATONVM_ZGC_LOAD_BARRIER`
    // arms the barrier outside tests (`ZgcRealHeap::arm_load_barrier_plain_stores`,
    // at heap construction).
    let armed_zgc =
        !(heap.is_generational() || heap.is_g1()) && cratonvm_types::zgc_read_barrier_armed();
    if armed_zgc && false
    {
        return None;
    }
    if heap.kind_of(fwd) != cratonvm_types::ObjectKind::Array {
        return None;
    }
    if heap.element_type_of_validated(fwd) != cratonvm_types::ArrayElementType::Reference
        || index >= heap.array_length(fwd)
    {
        return None;
    }
    let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();
    // G1 writes reference elements as raw 8-byte words; a narrow G1 heap is
    // refused at init, and is declined here rather than CAS-ed at the wrong
    // width if that ever changes.
    if narrow && !heap.is_generational() {
        return None;
    }
    let width = cratonvm_types::narrow_oop::ref_element_size();
    let exp_raw = exp.map_or(0u64, |r| r.as_ptr() as u64);
    let new_raw = new.map_or(0u64, |r| r.as_ptr() as u64);
    if narrow
        && !(cratonvm_types::narrow_oop::is_encodable(exp_raw)
            && cratonvm_types::narrow_oop::is_encodable(new_raw))
    {
        return None;
    }
    let base = heap.array_data_ptr(fwd)?;
    let byte_off = index.checked_mul(width)?;
    // SAFETY: `index < length`, so `byte_off + width` is inside the payload.
    let addr = unsafe { base.add(byte_off) };
    if (addr as usize) % width != 0 {
        return None;
    }
    // zt t32 (2f): the slot-aware form (the witness word's color decides).
    heap.satb_barrier_slot(addr as *const u8, expected);
    if armed_zgc {
        // SAFETY: as for the bit CAS below; the armed path also writes the
        // slot (the heal), which every reference slot of a live array allows.
        let (witness, swapped) = unsafe { heap.cas_ref_slot_barriered(addr, exp_raw, new_raw) }?;
        if swapped {
            ref_cas_post_barrier(shared, fwd, index, new_val);
        }
        return Some((witness, swapped));
    }
    // SAFETY: `addr` is a naturally aligned reference slot of the heap's
    // width inside a live, forwarded array that cannot move before this one
    // atomic returns (a native reaches no safepoint here). Every other access
    // to the slot is a plain or atomic machine access of the same width.
    let (witness, swapped) = unsafe {
        if narrow {
            let a = AtomicU32::from_ptr(addr.cast::<u32>());
            let e = cratonvm_types::narrow_oop::encode(exp_raw);
            let n = cratonvm_types::narrow_oop::encode(new_raw);
            match a.compare_exchange(e, n, SeqCst, SeqCst) {
                Ok(w) => (cratonvm_types::narrow_oop::decode(w), true),
                Err(w) => (cratonvm_types::narrow_oop::decode(w), false),
            }
        } else {
            let a = AtomicU64::from_ptr(addr.cast::<u64>());
            match a.compare_exchange(exp_raw, new_raw, SeqCst, SeqCst) {
                Ok(w) => (w, true),
                Err(w) => (w, false),
            }
        }
    };
    if swapped {
        ref_cas_post_barrier(shared, fwd, index, new_val);
    }
    Some((witness, swapped))
}

/// nio21 wave 6 L3: the NARROW compact reference field, the one reference
/// field shape [`hw_atomic_addr`] refuses (its compact arm wants an 8-byte
/// slot) on a live heap. Under compressed oops (generational only) a compact
/// reference field is a 4-byte encoded narrow oop, and a CAS on it used to fall
/// to the `with_cas_lock` read-compare-write, which a plain `putfield` (no lock)
/// can slip into: the lost update of the array page, for fields. This is one
/// `u32` `LOCK CMPXCHG` of `encode(expected)` / `encode(new)` on the slot, with
/// the field arm's barriers in its order.
///
/// `None` (the lock path serves) unless narrow oops are on, `fwd` is a
/// COMPACT instance whose slot `index` is a declared reference field with a
/// 4-byte narrow storage, both operands encode, and
/// `CRATONVM_UNSAFE_REF_ARRAY_HW_CAS` is on (the switch covers both new
/// reference arms).
pub(in crate::vm::vm_exec) fn hw_cas_narrow_reference_field(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    use cratonvm_types::narrow_oop;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
    let (Value::Object(exp), Value::Object(new)) = (expected, new_val) else {
        return None;
    };
    if !narrow_oop::narrow_oops_enabled()
        || !crate::runtime::env_cache::unsafe_ref_array_hw_cas_enabled()
    {
        return None;
    }
    let heap = &shared.mem.heap;
    if !heap.is_generational() || heap.kind_of(fwd) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let class_id = heap.class_id_of(fwd);
    if !HwAtomicKind::Reference.matches_descriptor(resolve_field_descriptor_byte_cached(
        shared, class_id, index,
    )?) {
        return None;
    }
    if heap.get_header(fwd).gc_flags() & cratonvm_types::GC_FLAG_COMPACT == 0 {
        return None;
    }
    let (disp, storage) = cratonvm_types::compact_field_disp_storage(class_id.as_u32(), index)?;
    if !matches!(storage, cratonvm_types::FieldStorageKind::Reference)
        || storage.size_runtime() as usize != narrow_oop::NARROW_REF_SIZE
    {
        return None;
    }
    let addr = (fwd.as_ptr() as usize).checked_add(disp)?;
    if addr % narrow_oop::NARROW_REF_SIZE != 0 {
        return None;
    }
    let exp_raw = exp.map_or(0u64, |r| r.as_ptr() as u64);
    let new_raw = new.map_or(0u64, |r| r.as_ptr() as u64);
    if !(narrow_oop::is_encodable(exp_raw) && narrow_oop::is_encodable(new_raw)) {
        return None;
    }
    heap.satb_barrier(expected);
    // SAFETY: `addr` is the 4-aligned narrow reference slot of a declared
    // field of a live, forwarded compact instance that cannot move before
    // this one atomic returns (a native reaches no safepoint here).
    let swapped = unsafe { AtomicU32::from_ptr(addr as *mut u32) }
        .compare_exchange(
            narrow_oop::encode(exp_raw),
            narrow_oop::encode(new_raw),
            SeqCst,
            SeqCst,
        )
        .is_ok();
    if swapped {
        ref_cas_post_barrier(shared, fwd, index, new_val);
    }
    Some(swapped)
}


/// The field compare-and-swap, as a free function over `&SharedVm`.
///
/// Extracted from `VmExec::compare_and_swap_field` so the JIT funnel's
/// `try_varhandle_instance_field_cas` can serve a `VarHandle.compareAndSet`
/// without a second copy of the barrier ordering. The read path already works
/// this way -- both of its routes call `varhandle_instance_field_read_bits` so
/// they cannot drift on what they consider servable -- and a CAS has more to
/// get wrong: the SATB pre-barrier fires on `expected` BEFORE the store, and
/// the post `write_barrier` only on success.
pub(crate) fn compare_and_swap_field_shared(
    shared: &SharedVm,
    obj: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> bool {
    // An `int`-to-`int` CAS goes to the hardware, for the same reason
    // `atomic_fetch_add_int` does: `AtomicInteger.compareAndSet` and a
    // compiled `LOCK XADD` must be atomic against each other, and a
    // lock-based read-compare-write is not. No SATB pre-barrier and no
    // write barrier here — the payload is primitive, so no reference is
    // overwritten and none escapes.
    let fwd = shared.mem.heap.load_and_forward(obj);
    let expected = forward_boundary_value(&shared.mem.heap, expected);
    let new_val = forward_boundary_value(&shared.mem.heap, new_val);
    if let Some(swapped) = hw_cas_primitive_field(shared, fwd, index, expected, new_val) {
        return swapped;
    }
    // nio21 wave 5 L3: a primitive-array element is one hardware CAS too, so
    // it is atomic against the plain element stores that take no lock (and
    // against a compiled `LOCK CMPXCHG` / `LOCK XADD` on the element).
    if let Some((_, swapped)) =
        hw_cas_primitive_array_element(shared, fwd, index, expected, new_val)
    {
        return swapped;
    }
    // JIT round 23 wave 1, lane L4 (W8-L3-1): every reference arm, the
    // slot-CAS fallback included, lives in `hw_cas_reference`, which the lock
    // path below re-asks under the CAS lock.
    if let Some(swapped) = hw_cas_reference(shared, fwd, index, expected, new_val) {
        return swapped;
    }
    // T19_H6: descriptor-aware CAS read+write so a long instance field
    // (`J`) always decodes as `Value::Long`, never as `Value::Double`.
    compare_and_swap_field_locked(shared, fwd, fwd, index, expected, new_val)
}

/// The hardware reference CAS of [`compare_and_swap_field_shared`]: the
/// wide field cell (`hw_atomic_addr`), the reference-array element
/// ([`hw_cas_reference_array_element`]), the narrow compact field
/// ([`hw_cas_narrow_reference_field`]) and, when all three decline, the slot
/// CAS ([`ref_slot_cas_fallback`]). `None` for a non-reference pair and for
/// every shape none of them serves; the caller then takes the lock path.
///
/// JIT round 23 wave 1, lane L4: extracted so the lock path can re-ask it
/// UNDER the CAS lock (as JIT round 22 wave 5c re-asks the primitive arm), so
/// a lock-path reference CAS and a hardware one of the same slot can never
/// both succeed.
pub(in crate::vm::vm_exec) fn hw_cas_reference(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    match (expected, new_val) {
        (Value::Object(exp), Value::Object(new)) => {
            // JIT round 20, lane atomics: while a ZGC load barrier is ARMED a
            // reference field may hold a colored word, and this bit CAS of
            // the uncolored `expected` would fail spuriously (a strong CAS
            // must not). The reference-array arm already screens it
            // (`hw_cas_reference_array_element`); the field arm did not.
            // Decline to the lock path, whose read decodes through the heap.
            let armed_zgc = !(shared.mem.heap.is_generational() || shared.mem.heap.is_g1())
                && cratonvm_types::zgc_read_barrier_armed();
            if let Some(addr) = (!armed_zgc)
                .then(|| hw_atomic_addr(shared, fwd, index, HwAtomicKind::Reference))
                .flatten()
            {
                // The payload word of a reference cell IS the raw pointer,
                // zero for a JVM null, and `values_equal_for_cas` compares
                // references by exactly that pointer — so a bit-level CAS
                // keeps the lock path's semantics.
                let exp_raw = exp.map_or(0usize, |r| r.as_ptr() as usize);
                let new_raw = new.map_or(0usize, |r| r.as_ptr() as usize);
                // SATB pre-barrier BEFORE the store, preserving (pre,
                // store, post) ordering. Firing it on `expected` rather
                // than on a re-read is what HotSpot does for a CAS: on
                // success `expected` IS the overwritten value, and on
                // failure the extra enqueue only over-approximates the
                // live set, which SATB is allowed to do.
                shared.mem.heap.satb_barrier(expected);
                // SAFETY: see `atomic_fetch_add_int`.
                let cell = unsafe { hw_atomic_usize(addr) };
                let swapped = cell
                    .compare_exchange(
                        exp_raw,
                        new_raw,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    )
                    .is_ok();
                if swapped {
                    // cards3 item 4 (gen r5w2/alloc6): `hw_atomic_addr`
                    // refuses arrays, so this is always the holder barrier;
                    // the element-card arm serves the array CAS below.
                    ref_cas_post_barrier(shared, fwd, index, new_val);
                }
                return Some(swapped);
            }
            // nio21 wave 6 L3: a reference-array element is one hardware CAS
            // on the slot, atomic against the plain element stores that take
            // no lock.
            if let Some((_, swapped)) =
                hw_cas_reference_array_element(shared, fwd, index, expected, new_val)
            {
                return Some(swapped);
            }
            // And the narrow compact reference field `hw_atomic_addr` refuses.
            if let Some(swapped) =
                hw_cas_narrow_reference_field(shared, fwd, index, expected, new_val)
            {
                return Some(swapped);
            }
            // JIT round 23 wave 1, lane L4 (W8-L3-1): what the three arms
            // decline (their kill switch, an unencodable narrow operand, an
            // armed ZGC heap under `CRATONVM_ZGC_ARMED_REF_CAS`) is still one
            // CAS on the slot, never the lock path's read-compare-write.
            ref_slot_cas_fallback(shared, fwd, index, expected, new_val)
        }
        _ => None,
    }
}

/// JIT round 23 wave 1, lane L4 (W8-L3-1): where a reference slot lives, for
/// [`ref_slot_cas_fallback`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::vm::vm_exec) enum RefCasSlot {
    /// A 4-byte encoded narrow oop (compressed oops; Generational only).
    Narrow(*mut u8),
    /// An 8-byte raw pointer word (`0` is null).
    Wide(*mut u8),
}

/// The reference slot `index` of `fwd` as the heap itself stores it, or
/// `None`: element `index` of a reference array (the heap's element width),
/// a wide reference field cell [`hw_atomic_addr`] proves (a legacy cell
/// already tagged `Object`, or an 8-byte compact slot), or a 4-byte narrow
/// compact reference field. Every other shape (an untagged legacy cell, an
/// unresolved descriptor, a misaligned slot) answers `None`.
pub(in crate::vm::vm_exec) fn ref_cas_slot(shared: &SharedVm, fwd: ObjectRef, index: usize) -> Option<RefCasSlot> {
    use cratonvm_types::narrow_oop;
    let heap = &shared.mem.heap;
    let narrow = narrow_oop::narrow_oops_enabled();
    if heap.kind_of(fwd) == cratonvm_types::ObjectKind::Array {
        if heap.element_type_of_validated(fwd) != cratonvm_types::ArrayElementType::Reference
            || index >= heap.array_length(fwd)
        {
            return None;
        }
        // G1 and ZGC store reference elements as 8-byte words and refuse
        // narrow oops at init; decline rather than CAS at the wrong width.
        if narrow && !heap.is_generational() {
            return None;
        }
        let width = narrow_oop::ref_element_size();
        let base = heap.array_data_ptr(fwd)?;
        // SAFETY: `index < length`, so the slot is inside the payload.
        let addr = unsafe { base.add(index.checked_mul(width)?) };
        if (addr as usize) % width != 0 {
            return None;
        }
        return Some(if narrow {
            RefCasSlot::Narrow(addr)
        } else {
            RefCasSlot::Wide(addr)
        });
    }
    if let Some(addr) = hw_atomic_addr(shared, fwd, index, HwAtomicKind::Reference) {
        return Some(RefCasSlot::Wide(addr));
    }
    let class_id = heap.class_id_of(fwd);
    let header = heap.get_header(fwd);
    if header.gc_flags() & cratonvm_types::GC_FLAG_COMPACT != 0 {
        // A compact body: the layout row itself says whether the slot is a
        // reference and how wide it is (JIT round 23 wave 2, lane L4: this no
        // longer needs the resolved descriptor `hw_atomic_addr` asks for, so a
        // transiently unresolved one -- `HwAtomicRefusal::DescriptorUnresolved`
        // -- no longer sends a reference CAS to the lock path).
        let (disp, storage) =
            cratonvm_types::compact_field_disp_storage(class_id.as_u32(), index)?;
        if !matches!(storage, cratonvm_types::FieldStorageKind::Reference) {
            return None;
        }
        let width = storage.size_runtime() as usize;
        let addr = (fwd.as_ptr() as usize).checked_add(disp)?;
        if addr % width != 0 {
            return None;
        }
        return match width {
            narrow_oop::NARROW_REF_SIZE if narrow && heap.is_generational() => {
                Some(RefCasSlot::Narrow(addr as *mut u8))
            }
            narrow_oop::WIDE_REF_SIZE if !narrow => Some(RefCasSlot::Wide(addr as *mut u8)),
            _ => None,
        };
    }
    legacy_ref_cell_payload(shared, fwd, class_id, index).map(RefCasSlot::Wide)
}

/// JIT round 23 wave 2, lane L4
/// (`docs/feature-designs/real-jdk-mode.md` item 3):
/// the payload word of LEGACY cell `index` of `fwd` for a reference CAS that
/// [`hw_atomic_addr`] refused, or `None`.
///
/// - A cell already tagged `Object` is a reference cell by its own tag, whether
///   or not the slot's descriptor resolves yet.
/// - An all-zero cell (tag `Int`, payload 0: a reference slot allocated
///   without descriptors) of a declared `L` / `[` field is first tagged
///   `Object(None)` by ONE compare-exchange of its tag word (`0` ->
///   `FIELD_CELL_TAG_OBJECT`; the payload stays 0). For a reference
///   descriptor both encodings read as null, so no reader can tell; a racing
///   plain `putfield` writes the same tag (its payload store follows), so the
///   exchange either wins or finds the store's tag -- either way the cell is
///   then a tagged reference cell and the payload CAS that follows is
///   linearizable against the store.
///
/// Anything else (a primitive tag with a non-zero payload, a slot past the
/// cell count, a misaligned cell) answers `None`.
pub(in crate::vm::vm_exec) fn legacy_ref_cell_payload(
    shared: &SharedVm,
    fwd: ObjectRef,
    class_id: ClassId,
    index: usize,
) -> Option<*mut u8> {
    use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
    let header = shared.mem.heap.get_header(fwd);
    if index >= header.num_slots() as usize {
        return None;
    }
    let cell = (fwd.as_ptr() as usize)
        .checked_add(cratonvm_types::HEADER_SIZE)?
        .checked_add(index.checked_mul(cratonvm_types::SLOT_SIZE)?)?;
    if cell % 8 != 0 {
        return None;
    }
    let tag_object = u64::from(cratonvm_types::FIELD_CELL_TAG_OBJECT);
    // SAFETY: `index < num_slots` on a legacy body, so `cell` is the start of
    // a live, 8-aligned 16-byte `Value` cell; its two words are only ever
    // accessed as 8-byte atomics (`write_value_atomic` / `read_value_atomic`)
    // or by hardware atomics of the payload.
    let (w0, w1) = unsafe {
        (
            AtomicU64::from_ptr(cell as *mut u64),
            AtomicU64::from_ptr(
                (cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *mut u64,
            ),
        )
    };
    let tag = w0.load(SeqCst) & 0xffff_ffff;
    if tag != tag_object {
        let declared_reference = resolve_field_descriptor_byte_cached(shared, class_id, index)
            .is_some_and(|d| HwAtomicKind::Reference.matches_descriptor(d));
        if !declared_reference || w0.load(SeqCst) != 0 || w1.load(SeqCst) != 0 {
            return None;
        }
        match w0.compare_exchange(0, tag_object, SeqCst, SeqCst) {
            Ok(_) => {}
            Err(now) if now & 0xffff_ffff == tag_object => {}
            Err(_) => return None,
        }
    }
    Some((cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET) as *mut u8)
}

/// How [`ref_slot_cas_fallback`] treats a reference slot of an ARMED ZGC heap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::vm::vm_exec) enum ArmedRefCasRoute {
    /// The lock path (`compare_and_swap_field_locked`).
    Decline,
    /// The heal-then-CAS (`cas_ref_slot_healed`): the slot's writers store
    /// coloured words.
    Healed,
    /// One machine-word compare-exchange, as on an unarmed heap.
    Plain,
}

/// [`ArmedRefCasRoute`] of an armed slot.
///
/// * Reference-array ELEMENTS: every writer stores a coloured word since JIT
///   r23 wave 2, so they take the heal-then-CAS under
///   `CRATONVM_ZGC_ARMED_REF_CAS` (default ON), else the lock.
/// * Reference FIELDS (JIT r23 wave 6, lane L5,
///   `r23-w1-ref-cas-lock-path-residuals-FIXED-20261021.md`;
///   `CRATONVM_ZGC_ARMED_FIELD_REF_CAS_PLAIN`, correctness, default ON, `=0`
///   restores the lock path): an armed heap still stores AND reads field slots
///   as plain machine pointers (`set_field` / `get_field` take no colour; the
///   arming plan's A2a/A2b colour them later). The lock path's
///   read-compare-write under `with_cas_lock` excludes only other lock-path
///   CASes, so a plain `putfield` landing between its read and its write was
///   lost while the CAS reported success -- the defect this page tracked. A
///   plain compare-exchange of the machine word is exactly what the slot's
///   writers and readers agree on while fields are uncoloured, and is atomic
///   against every one of them.
/// * Reference FIELDS of a COLORED-STORE heap (zt t31/zc11;
///   `CRATONVM_ZGC_LOAD_BARRIER=colored`, `fields_colored`): `set_field`
///   writes `heal_color() | offset` into an 8-byte compact reference slot
///   (zt t19/zb2 A2b), so the plain word CAS of an uncoloured `expected`
///   never matches and a Java CAS retry loop spun forever
///   (`Zt31Zc11RoundStress` under the color flip and the region quarantine).
///   Such a field takes the elements' heal-then-CAS, under the elements'
///   switch: every writer of the slot stores a coloured word or a plain one
///   the heal answers verbatim.
pub(in crate::vm::vm_exec) fn armed_ref_cas_route(
    is_array: bool,
    fields_colored: bool,
    element_healed_on: bool,
    field_plain_on: bool,
) -> ArmedRefCasRoute {
    match (is_array || fields_colored, element_healed_on, field_plain_on) {
        (true, true, _) => ArmedRefCasRoute::Healed,
        (true, false, _) => ArmedRefCasRoute::Decline,
        (false, _, true) => ArmedRefCasRoute::Plain,
        (false, _, false) => ArmedRefCasRoute::Decline,
    }
}


/// JIT round 23 wave 1, lane L4
/// (`docs/feature-designs/jit-ir-optimizations.md` W8-L3-1,
/// `CRATONVM_REF_CAS_SLOT_LOOP`, default ON, `=0` restores the lock path):
/// the reference CAS the three hardware arms of [`hw_cas_reference`] decline,
/// as ONE compare-exchange on the slot instead of the `with_cas_lock`
/// read-compare-write, which a plain `aastore` / `putfield` /
/// `set_array_element` (none takes that lock) can slip into: the store is
/// lost and the CAS reports success, which HotSpot's `LOCK CMPXCHG` never
/// does.
///
/// The arms decline in three cases, all served here:
/// - their kill switch `CRATONVM_UNSAFE_REF_ARRAY_HW_CAS=0` (this switch is
///   separate, so `=0` on both is the old lock path for an A/B);
/// - an UNENCODABLE narrow operand. An unencodable `expected` names no
///   object a narrow slot can hold, so the CAS fails, and one atomic load of
///   the slot is its linearization point (no write, no barrier). An
///   unencodable `new` with a matching slot has no correct store at all
///   (`narrow_oop::encode` panics, on the lock path too): it fails at one load
///   when the slot does not hold `expected`, and otherwise reports the
///   geometry fault here rather than on the lock path (JIT round 23 wave 2);
/// - an ARMED ZGC heap, for a reference-array ELEMENT: heal-then-CAS
///   (`VmHeap::cas_ref_slot_barriered`) under `CRATONVM_ZGC_ARMED_REF_CAS`
///   (default ON since JIT round 23 wave 2: every armed element writer now
///   stores a coloured word, `ZgcRealHeap::armed_store_ref_element`). An armed
///   FIELD still declines: ZGC field stores and interpreted field reads take
///   no colour at all yet (the arming plan's field half), so a heal-then-CAS
///   there could decode a plain pointer as another object (see
///   [`armed_ref_cas_route`] for the fields' current routes). Only the opt-in
///   `CRATONVM_ZGC_LOAD_BARRIER` arms the barrier outside tests
///   (`r21-armed-zgc-reference-cas-needs-heal-then-cas-CLOSED-20261012.md`).
///
/// The narrow encoding is a bijection on encodable addresses, so "read the
/// word, decode, compare, CAS the word read" is one `LOCK CMPXCHG` of
/// `encode(expected)`: a retry could only follow a word that decodes to
/// `expected` yet differs from `encode(expected)`, which does not exist.
/// Barriers are the hardware arms': SATB on `expected` before the store, the
/// element card / holder barrier on success ([`ref_cas_post_barrier`]).
pub(in crate::vm::vm_exec) fn ref_slot_cas_fallback(
    shared: &SharedVm,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> Option<bool> {
    use cratonvm_types::narrow_oop;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
    let (Value::Object(exp), Value::Object(new)) = (expected, new_val) else {
        return None;
    };
    if !shared.hw_ref_cas_slot_loop {
        return None;
    }
    let heap = &shared.mem.heap;
    let armed =
        !(heap.is_generational() || heap.is_g1()) && cratonvm_types::zgc_read_barrier_armed();
    let route = if armed {
        armed_ref_cas_route(
            heap.kind_of(fwd) == cratonvm_types::ObjectKind::Array,
            heap.zgc_load_barrier_colored_stores(),
            true,
            true,
        )
    } else {
        ArmedRefCasRoute::Plain
    };
    // `armed_zgc` below means "the slot holds coloured words": the healed arm.
    let armed_zgc = match route {
        ArmedRefCasRoute::Decline => return None,
        ArmedRefCasRoute::Healed => true,
        ArmedRefCasRoute::Plain => false,
    };
    let slot = ref_cas_slot(shared, fwd, index)?;
    let exp_raw = exp.map_or(0u64, |r| r.as_ptr() as u64);
    let new_raw = new.map_or(0u64, |r| r.as_ptr() as u64);
    match slot {
        RefCasSlot::Wide(addr) => {
            // zt t32 (2f): the slot-aware form.
            heap.satb_barrier_slot(addr as *const u8, expected);
            let swapped = if armed_zgc {
                // SAFETY: an 8-aligned reference slot of a live, forwarded
                // object, valid for writes (the heal), that cannot move
                // before this call returns (a native reaches no safepoint).
                unsafe { heap.cas_ref_slot_barriered(addr, exp_raw, new_raw) }?.1
            } else {
                // SAFETY: as above; `ref_cas_slot` proved the 8-byte width
                // and the alignment, and every other access of the slot is a
                // plain or atomic machine access of the same width.
                unsafe { AtomicU64::from_ptr(addr.cast::<u64>()) }
                    .compare_exchange(exp_raw, new_raw, SeqCst, SeqCst)
                    .is_ok()
            };
            if swapped {
                ref_cas_post_barrier(shared, fwd, index, new_val);
            }
            Some(swapped)
        }
        RefCasSlot::Narrow(addr) => {
            if armed_zgc {
                // A coloured word does not fit a 4-byte slot (vm_init refuses
                // the combination); never reached.
                return None;
            }
            // SAFETY: a 4-aligned narrow reference slot of a live, forwarded
            // object that cannot move before this one atomic returns.
            let cell = unsafe { AtomicU32::from_ptr(addr.cast::<u32>()) };
            if !narrow_oop::is_encodable(exp_raw) {
                // No narrow slot holds an unencodable address: the CAS fails
                // at this load. (Null always encodes, so a slot holding null
                // cannot be the witness of a match here.)
                let _ = cell.load(SeqCst);
                return Some(false);
            }
            // JIT round 23 wave 2, lane L4 (residual item 2): an unencodable
            // `new` is NOT declined to the lock path any more. No slot can
            // hold it, so no path has a correct store: `narrow_oop::encode`
            // below reports it (`FATAL narrow-oop encode failure`) and
            // panics, exactly as the lock path's `set_array_element` /
            // `set_field` (and every `aastore`) do -- a geometry fault, never
            // a silent read-compare-write. A slot that does not hold
            // `expected` fails the CAS at one load first, as the lock path
            // fails it without storing.
            if !narrow_oop::is_encodable(new_raw) && cell.load(SeqCst) != narrow_oop::encode(exp_raw)
            {
                return Some(false);
            }
            heap.satb_barrier(expected);
            let swapped = cell
                .compare_exchange(
                    narrow_oop::encode(exp_raw),
                    narrow_oop::encode(new_raw),
                    SeqCst,
                    SeqCst,
                )
                .is_ok();
            if swapped {
                ref_cas_post_barrier(shared, fwd, index, new_val);
            }
            Some(swapped)
        }
    }
}

/// The lock path of [`compare_and_swap_field_shared`]: the `Value`-coercion
/// pairs (an `Object(None)` against a typed zero, a primitive into a
/// reference array's autobox) and the shapes no hardware arm serves, as a
/// read, a compare and a write under `monitors.with_cas_lock` (plus the
/// stripe lock for a field). Re-asks the hardware arms under the lock.
pub(in crate::vm::vm_exec) fn compare_and_swap_field_locked(
    shared: &SharedVm,
    obj: ObjectRef,
    fwd: ObjectRef,
    index: usize,
    expected: Value,
    new_val: Value,
) -> bool {
    let is_array = shared.mem.heap.kind_of(obj) == cratonvm_types::ObjectKind::Array;
    let class_id = shared.mem.heap.class_id_of(obj);
    let descriptor = if is_array {
        None
    } else {
        resolve_field_descriptor_byte_cached(shared, class_id, index)
    };
    let swapped = shared.threads.monitors.with_cas_lock(obj, || {
        // JIT round 22 wave 5c, lane sync (review `r22-review-w5-memory-on-sync`
        // item 2): the hardware arms above refused a cell that was not yet
        // tagged; if a store tagged it since (the field's first store), CAS it
        // in the hardware domain here too, so a lock-path CAS and a hardware
        // CAS of the same cell can never both succeed. Under the CAS lock, so
        // another lock-path CAS is excluded as before.
        if !is_array {
            if let Some(swapped) = hw_cas_primitive_field(shared, fwd, index, expected, new_val) {
                return swapped;
            }
        }
        // JIT round 23 wave 1, lane L4: the same for a reference pair (a
        // legacy cell tagged `Object` by a store since the first ask). The
        // arm applies its own barriers; the holder barrier after the lock
        // repeats one card mark, which is idempotent.
        if let Some(swapped) = hw_cas_reference(shared, fwd, index, expected, new_val) {
            return swapped;
        }
        // The CAS mutex provides the operation's per-object
        // linearization point, but ordinary volatile readers and writers
        // use the collector's stripe lock to prevent tearing the 16-byte
        // `Value` slot.  Hold that stripe once across the whole
        // read/compare/write rather than calling `get_field_volatile_as`
        // and `set_field_volatile_as`, which acquire it twice and insert
        // four SeqCst fences for one successful Unsafe CAS.  The single
        // pair of fences below retains the full volatile/CAS ordering, and
        // the shared stripe keeps this raw slot access atomic with every
        // non-CAS volatile access.
        let volatile_guard =
            (!is_array).then(|| cratonvm_gc::collector::volatile_stripe_lock(obj, index));
        if volatile_guard.is_some() {
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        }
        let current = if is_array {
            shared
                .mem
                .heap
                .get_array_element(obj, index)
                .unwrap_or(Value::Object(None))
        } else if let Some(desc) = descriptor {
            shared.mem.heap.get_field_as(obj, index, desc)
        } else {
            shared.mem.heap.get_field(obj, index)
        };
        let swapped = if values_equal_for_cas(&current, &expected) {
            // Task #42 (deferred from #25): SATB pre-barrier on
            // the CAS-putfield / CAS-aastore path.  Without it,
            // a successful CAS that overwrites an old ref slot
            // between G1 initial-mark and remark would silently
            // drop the old reference from the live closure —
            // the same lost-object scenario the interpreter's
            // putfield/aastore paths already guard against. We
            // dispatch through `VmHeap::satb_barrier` (the
            // available API on this branch); `satb_barrier`
            // short-circuits on null and on non-Object payloads,
            // so primitive `Unsafe.compareAndSwapInt/Long` and
            // null→x CAS pays effectively nothing.  Fires
            // strictly BEFORE the store to preserve SATB
            // (pre, store, post) ordering. On a non-G1
            // generational backend the call is also a cheap
            // tag-test no-op.
            //
            // gci i18/sw (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015`
            // item 4, the lock-path residual): not on G1, whose store
            // accessors below (`set_array_element`, `set_field_as` ->
            // `set_field`, `set_field`) read and log the overwritten value
            // themselves while the queue is active, so this call logged it a
            // second time -- the interpreter's `op_putfield` / `aastore` skip
            // it on G1 the same way since gci i12/wb.
            if !shared.mem.heap.is_g1() {
                shared.mem.heap.satb_barrier(current);
            }
            if is_array {
                let _ = shared.mem.heap.set_array_element(obj, index, new_val);
            } else if let Some(desc) = descriptor {
                shared.mem.heap.set_field_as(obj, index, new_val, desc);
            } else {
                shared.mem.heap.set_field(obj, index, new_val);
            }
            true
        } else {
            false
        };
        if volatile_guard.is_some() {
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        }
        swapped
    });
    if swapped {
        if crate::runtime::env_cache::dbg_loader_trace() {
            if let Value::Object(Some(o)) = new_val {
                let new_cid = shared.mem.heap.class_id_of(o);
                let cn = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(new_cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                if cn.contains("RootReference") {
                    let holder_cid = shared.mem.heap.class_id_of(obj);
                    let holder_cn = shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(holder_cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    eprintln!(
                        "[LOADER-TRACE] compare_and_swap_field SUCCESS holder_obj={:p} holder_class={} slot={} new_obj={:p} new_class={} new_cid={}",
                        obj.as_ptr(), holder_cn, index, o.as_ptr(), cn, new_cid.as_u32()
                    );
                }
            }
        }
        // cards3 item 3 producer (f), gcd d3/n: on Generational an array
        // store above went through `set_array_element`, which already
        // dirtied the ELEMENT's card; this header mark made the next young
        // pause read the whole array.
        if !(is_array && shared.mem.heap.is_generational()) {
            shared.mem.heap.write_barrier(obj, new_val);
        }
    }
    // T19.H7 diag: count CAS failures so we can spot a livelock.
    // Static counter gated to ~5 emissions then 1 every 1M.
    // Feature-gated (off by default) вЂ” see vm/Cargo.toml
    // `experimental-t19-diag`. Re-enable with
    // `--features experimental-t19-diag`.
    #[cfg(feature = "experimental-t19-diag")]
    if !swapped {
        static CAS_FAIL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = CAS_FAIL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n < 5 || n % 1_000_000 == 0 {
            let cn = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_else(|| format!("cid={}", class_id.as_u32()));
            tracing::debug!(
                target: "cratonvm::t19_h7_cas",
                "CAS FAIL #{n} class={cn} slot={index} desc={:?} expected={:?} new={:?}",
                descriptor.map(|b| b as char), expected, new_val,
            );
        }
    }
    swapped
}


/// `CRATONVM_G1_DBG_GRAY_PROV` (nio21 wave 17: it was a declared-flag read
/// every 1024th reference element of every array store). Read once per VM
/// (`SharedVm::dbg_g1_gray_prov`, gci i14/sx), no longer a process latch.
#[inline]
pub(in crate::vm::vm_exec) fn g1_gray_prov_dbg(shared: &SharedVm) -> bool {
    shared.dbg_g1_gray_prov
}

/// Entry `index` (1-based, as the class file numbers it) of a lambda proxy's
/// modelled spun-class pool (`lambda_proxy::SpunClass::pool`, entry 1 first)
/// in the form the `jdk.internal.reflect.ConstantPool` natives read
/// (interpreter round i5 wave 107, lane L3; `L3W106LambdaProxyConstantPool`).
/// The model has no `long` / `double` entries, so every index is one slot.
pub(in crate::vm::vm_exec) fn lambda_proxy_reflect_constant(
    pool: &[crate::runtime::lambda_proxy::PoolEntry],
    index: u16,
) -> cratonvm_native_api::ReflectConstant {
    use crate::runtime::lambda_proxy::PoolEntry as P;
    use cratonvm_native_api::ReflectConstant as R;
    fn at(pool: &[P], i: u16) -> Option<&P> {
        pool.get(usize::from(i).checked_sub(1)?)
    }
    fn utf8(pool: &[P], i: u16) -> Option<&str> {
        match at(pool, i)? {
            P::Utf8(text) => Some(text.as_str()),
            _ => None,
        }
    }
    let class_name = |i: u16| -> Option<String> {
        match at(pool, i)? {
            P::Class(name) => utf8(pool, *name).map(str::to_string),
            _ => None,
        }
    };
    let name_and_type = |i: u16| -> Option<(String, String)> {
        match at(pool, i)? {
            P::NameAndType(n, t) => utf8(pool, *n)
                .zip(utf8(pool, *t))
                .map(|(n, t)| (n.to_string(), t.to_string())),
            _ => None,
        }
    };
    let utf8 = |i: u16| utf8(pool, i);
    let Some(entry) = at(pool, index) else {
        return R::Invalid;
    };
    match entry {
        P::Utf8(text) => R::Utf8(text.encode_utf16().collect()),
        P::Integer(v) => R::Integer(*v),
        P::Class(name) => utf8(*name)
            .map(|n| R::Class(n.to_string()))
            .unwrap_or(R::Other(7)),
        P::String(text) => utf8(*text)
            .map(|t| R::String(t.encode_utf16().collect()))
            .unwrap_or(R::Other(8)),
        P::Fieldref(c, nt) | P::Methodref(c, nt) | P::InterfaceMethodref(c, nt) => {
            let tag = match entry {
                P::Fieldref(..) => 9,
                P::Methodref(..) => 10,
                _ => 11,
            };
            match (class_name(*c), name_and_type(*nt)) {
                (Some(class_name), Some((name, descriptor))) => R::MemberRef {
                    tag,
                    class_index: *c,
                    name_and_type_index: *nt,
                    class_name,
                    name,
                    descriptor,
                },
                _ => R::Other(tag),
            }
        }
        P::NameAndType(n, t) => match (utf8(*n), utf8(*t)) {
            (Some(name), Some(descriptor)) => R::NameAndType {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
            },
            _ => R::Other(12),
        },
        P::MethodHandle(..) => R::Other(15),
        P::Dynamic(_, nt) => R::DynamicRef {
            tag: 17,
            name_and_type_index: *nt,
        },
    }
}


/// Move the `w`-byte unit at byte offset `off` (`w` = 2, 4 or 8) with one load
/// and one store.
///
/// # Safety
/// `src + off` / `dst + off` are valid for `w` bytes and aligned to `w`.
#[inline(always)]
pub(in crate::vm::vm_exec) unsafe fn bulk_copy_move_unit(src: *const u8, dst: *mut u8, off: usize, w: usize) {
    match w {
        8 => {
            let v = std::ptr::read_volatile(src.add(off) as *const u64);
            std::ptr::write_volatile(dst.add(off) as *mut u64, v);
        }
        4 => {
            let v = std::ptr::read_volatile(src.add(off) as *const u32);
            std::ptr::write_volatile(dst.add(off) as *mut u32, v);
        }
        _ => {
            let v = std::ptr::read_volatile(src.add(off) as *const u16);
            std::ptr::write_volatile(dst.add(off) as *mut u16, v);
        }
    }
}

/// `bytes` bytes from `src` to `dst` (memmove semantics: the ranges may
/// overlap) with every `stride`-byte array element moved WHOLE, as HotSpot's
/// `conjoint_jlongs_atomic` / `_jints_atomic` / `_jshorts_atomic` move it, so
/// a thread writing the source concurrently is never observed torn
/// (JIT round 20 wave 15, lane fuzz14c; `R20FuzzGTearCopy1`). The units are
/// 8 bytes when both ends are 8-aligned (an aligned 8-byte unit holds whole
/// 2/4/8-byte elements), else the element size; the copy runs backwards when
/// `dst` overlaps the source above it. Volatile accesses keep the compiler
/// from re-forming a byte `memcpy`. Byte arrays, and the (never expected)
/// element-misaligned case, keep the plain `memmove`.
///
/// # Safety
/// `src` is valid for reads and `dst` for writes of `bytes` bytes, and
/// `bytes` is a multiple of `stride`.
///
/// `pub(crate)` since JIT round 21 wave 3 (lane intrinsics): the optimizing
/// tier's direct `System.arraycopy` helper (`jit/helpers.rs`
/// `try_primitive_arraycopy`) copies through it too.
pub(crate) unsafe fn bulk_copy_element_atomic(
    src: *const u8,
    dst: *mut u8,
    bytes: usize,
    stride: usize,
) {
    let (s, d) = (src as usize, dst as usize);
    if !matches!(stride, 2 | 4 | 8) || s % stride != 0 || d % stride != 0 {
        std::ptr::copy(src, dst, bytes);
        return;
    }
    let unit = if s % 8 == 0 && d % 8 == 0 { 8 } else { stride };
    let body = bytes - bytes % unit;
    if d > s && d < s + bytes {
        let mut off = bytes;
        while off > body {
            off -= stride;
            bulk_copy_move_unit(src, dst, off, stride);
        }
        while off > 0 {
            off -= unit;
            bulk_copy_move_unit(src, dst, off, unit);
        }
    } else {
        let mut off = 0;
        while off < body {
            bulk_copy_move_unit(src, dst, off, unit);
            off += unit;
        }
        while off < bytes {
            bulk_copy_move_unit(src, dst, off, stride);
            off += stride;
        }
    }
}

