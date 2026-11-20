#[allow(unused_imports)]
use super::*;

impl NativeFrameRow {
    /// Record the JNI native declared by `declaring` that is about to be
    /// called through `fn_ptr`, at `thread`'s current interpreter depth.
    ///
    /// # Safety
    ///
    /// `thread` is the calling thread's live `JvmThread`, and outlives the
    /// row.
    #[inline]
    pub(crate) unsafe fn enter_jni(
        thread: *mut JvmThread,
        declaring: ClassId,
        fn_ptr: usize,
    ) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let depth = thread_ref.frames.len();
        let jit_depth = jit_chain_len();
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        // Widening: a function address into the key (bit 63 clear).
        rows.push((depth, declaring, fn_ptr as u64, jit_depth));
        Self { thread, base }
    }

    /// Record the native whose [`native_row_method_hash`] is `hash`, declared
    /// by `declaring`, at `thread`'s current interpreter depth, around a
    /// method-event post; nothing when the innermost row already names it at
    /// this depth (a JNI arm's own row, found through `shared`'s JNI table).
    ///
    /// # Safety
    ///
    /// As [`Self::enter_jni`].
    pub(crate) unsafe fn enter_hashed(
        shared: &SharedVm,
        thread: *mut JvmThread,
        declaring: ClassId,
        hash: u64,
    ) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let depth = thread_ref.frames.len();
        let key = hash | ROW_KEY_IS_HASH;
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        let recorded = match rows.last() {
            Some(&(at, class_id, top, _)) if at == depth && class_id == declaring => {
                top == key
                    || (top & ROW_KEY_IS_HASH == 0
                        && shared
                            .classes
                            .class_manager
                            .read_recursive()
                            .get_class(declaring)
                            .is_some_and(|class| {
                                row_method_index(shared, class, top).is_some_and(|i| {
                                    class.methods.get(i).is_some_and(|m| {
                                        native_row_method_hash(&m.name, &m.descriptor) == hash
                                    })
                                })
                            }))
            }
            _ => false,
        };
        if !recorded {
            rows.push((depth, declaring, key, jit_chain_len()));
        }
        Self { thread, base }
    }

    /// Record that a call on lambda proxy `proxy` (a class the VM mints
    /// outside the class store) enters its method `index` -- the functional
    /// method or a bridge (`runtime::lambda_proxy::lambda_proxy_forwarding_index`)
    /// -- at `thread`'s current interpreter depth (interpreter round i2 wave
    /// 60, lane L1; `DebuggerGates::c_lambda_frames_armed`). The VM runs the
    /// spun method without a frame of its own (`interpreter::try_lambda_dispatch`
    /// calls the implementation directly); the row places one where HotSpot
    /// has it, right below the first frame the call puts on the stack, as a
    /// JNI native's row is placed ([`listed_rows`]). Only while a C JVMTI env
    /// is bound.
    ///
    /// # Safety
    ///
    /// As [`Self::enter_jni`].
    pub(crate) unsafe fn enter_lambda_proxy(
        thread: *mut JvmThread,
        proxy: ClassId,
        index: u16,
    ) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let depth = thread_ref.frames.len();
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        rows.push((
            depth,
            proxy,
            ROW_KEY_IS_PROXY | u64::from(index),
            jit_chain_len(),
        ));
        Self { thread, base }
    }
}

/// The calling thread's JIT entry-chain length, as a row records it (wave 44,
/// lane L3). Saturating: a chain that long cannot exist.
#[inline]
pub(in crate::jvmti::native_env) fn jit_chain_len() -> u32 {
    u32::try_from(crate::jit::conservative_roots::current_thread_jit_depth()).unwrap_or(u32::MAX)
}

impl Drop for NativeFrameRow {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `thread` outlives the row (`enter_jni`'s contract).
        unsafe { (*self.thread).jni_native_frames.truncate(self.base) };
    }
}

/// The bit position, in a lambda proxy row's key, of the number of its
/// locals record (`JvmThread::lambda_proxy_locals` index plus one; 0: none),
/// above the method index ([`proxy_row_index`]) and below
/// [`ROW_KEY_IS_PROXY`] (the i54-L1 retirement, [`LambdaProxyFrame`]).
pub(in crate::jvmti::native_env) const ROW_KEY_PROXY_LOCALS_SHIFT: u32 = 16;

/// The `JvmThread::lambda_proxy_locals` index a lambda proxy row's `key`
/// names, `None` for a row with no record and for any other row.
#[inline]
pub(in crate::jvmti::native_env) fn proxy_row_locals(key: u64) -> Option<usize> {
    proxy_row_index(key)?;
    let number = (key >> ROW_KEY_PROXY_LOCALS_SHIFT) & 0xFFFF_FFFF;
    usize::try_from(number).ok()?.checked_sub(1)
}

/// A lambda proxy method's frame while a call runs through it under a C
/// JVMTI agent (the i54-L1 retirement): wave 60's row
/// ([`NativeFrameRow::enter_lambda_proxy`]) plus the frame's locals, which
/// HotSpot's spun frame holds -- the proxy in slot 0, then the functional
/// method's arguments as the caller passed them (MEASURED,
/// `tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.java`).
/// Each reference slot is pinned in `JvmThread::native_pin_roots` for the
/// call (a null one by the proxy itself, marked null, so a write replaces
/// the pin in place), as the spun frame keeps them reachable; the
/// local-variable functions read and write them ([`proxy_local_read`],
/// [`proxy_local_write`]). `Drop` takes the record and the pins back, on
/// every exit edge.
pub(crate) struct LambdaProxyFrame {
    pub(in crate::jvmti::native_env) _row: NativeFrameRow,
    pub(in crate::jvmti::native_env) thread: *mut JvmThread,
    pub(in crate::jvmti::native_env) pin_base: usize,
    pub(in crate::jvmti::native_env) locals_base: usize,
}

impl LambdaProxyFrame {
    /// Enter lambda proxy `proxy`'s method `index` on `receiver` with the
    /// call's `args` (the functional method's, or a bridge's, as passed).
    ///
    /// # Safety
    ///
    /// As [`NativeFrameRow::enter_jni`].
    pub(crate) unsafe fn enter(
        thread: *mut JvmThread,
        proxy: ClassId,
        index: u16,
        receiver: ObjectRef,
        args: &[Value],
    ) -> Self {
        use crate::threading::jvm_thread::{LambdaProxyLocals, ProxyLocalSlot};
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let pins = &mut thread_ref.native_pin_roots;
        let pin_base = pins.len();
        let mut pin = |obj: ObjectRef| {
            pins.push(obj);
            pins.len() - 1
        };
        let mut slots = Vec::with_capacity(args.len() + 1);
        slots.push(ProxyLocalSlot::Ref {
            pin: pin(receiver),
            null: false,
        });
        for arg in args {
            match *arg {
                Value::Long(v) => slots.extend([ProxyLocalSlot::Long(v), ProxyLocalSlot::Second]),
                Value::Double(v) => {
                    slots.extend([ProxyLocalSlot::Double(v), ProxyLocalSlot::Second])
                }
                Value::Float(v) => slots.push(ProxyLocalSlot::Float(v)),
                Value::Object(Some(obj)) => slots.push(ProxyLocalSlot::Ref {
                    pin: pin(obj),
                    null: false,
                }),
                Value::Object(None) => slots.push(ProxyLocalSlot::Ref {
                    pin: pin(receiver),
                    null: true,
                }),
                Value::Int(v) => slots.push(ProxyLocalSlot::Int(v)),
                // Never a call argument.
                Value::ReturnAddress(_) | Value::Uninitialized => {
                    slots.push(ProxyLocalSlot::Second)
                }
            }
        }
        let locals_base = thread_ref.lambda_proxy_locals.len();
        thread_ref
            .lambda_proxy_locals
            .push(LambdaProxyLocals { slots });
        let depth = thread_ref.frames.len();
        let rows = &mut thread_ref.jni_native_frames;
        let base = rows.len();
        // Widening: the record's number into the key's middle bits.
        let number = (locals_base as u64 + 1) << ROW_KEY_PROXY_LOCALS_SHIFT;
        rows.push((
            depth,
            proxy,
            ROW_KEY_IS_PROXY | number | u64::from(index),
            jit_chain_len(),
        ));
        Self {
            _row: NativeFrameRow { thread, base },
            thread,
            pin_base,
            locals_base,
        }
    }
}

impl LambdaProxyFrame {
    /// The frame's receiver and arguments at their current addresses (a
    /// collection while an event's callback or a debugger park ran may have
    /// moved them), from the pins [`Self::enter`] made. `None` when the record
    /// is gone (never, while the frame lives).
    pub(crate) fn current(&self) -> Option<(ObjectRef, Vec<Value>)> {
        // SAFETY: `thread` outlives the frame (`enter`'s contract); only read.
        let thread = unsafe { &*self.thread };
        let mut values = proxy_frame_values(
            &thread.lambda_proxy_locals,
            &thread.native_pin_roots,
            ROW_KEY_IS_PROXY | ((self.locals_base as u64 + 1) << ROW_KEY_PROXY_LOCALS_SHIFT),
        )?;
        let Value::Object(Some(receiver)) = values.first().copied()? else {
            return None;
        };
        // The arguments as passed: one value each, a `long`'s or a
        // `double`'s second slot dropped.
        values.remove(0);
        values.retain(|v| !matches!(v, Value::Uninitialized));
        Some((receiver, values))
    }
}

impl Drop for LambdaProxyFrame {
    fn drop(&mut self) {
        // SAFETY: `thread` outlives the frame (`enter`'s contract).
        let thread = unsafe { &mut *self.thread };
        thread.lambda_proxy_locals.truncate(self.locals_base);
        thread.native_pin_roots.truncate(self.pin_base);
    }
}

/// The locals of the lambda proxy frame a row's `key` names, each
/// reference at its current (pinned) address and a `long`'s or a
/// `double`'s second slot `Uninitialized`, from a thread's own records and
/// pins: what a debugger's listing made on that thread publishes
/// (`interpreter::ListingSplice::values`; the i54-L1 retirement). `None`
/// for a row with no record.
pub(crate) fn proxy_frame_values(
    records: &[crate::threading::jvm_thread::LambdaProxyLocals],
    pins: &[ObjectRef],
    key: u64,
) -> Option<Vec<Value>> {
    use crate::threading::jvm_thread::ProxyLocalSlot as S;
    let record = records.get(proxy_row_locals(key)?)?;
    record
        .slots
        .iter()
        .map(|slot| {
            Some(match *slot {
                S::Int(v) => Value::Int(v),
                S::Long(v) => Value::Long(v),
                S::Float(v) => Value::Float(v),
                S::Double(v) => Value::Double(v),
                S::Ref { pin, null } => {
                    Value::Object(if null { None } else { Some(*pins.get(pin)?) })
                }
                S::Second => Value::Uninitialized,
            })
        })
        .collect()
}

/// Note the listing facts of method `index` of lambda proxy `proxy`
/// (`DebugRealm::lambda_proxy_rows`) the first time a recorded call enters
/// it, on the calling thread, which may read the class store (the i54-L1
/// retirement). One lock and lookup afterwards; only while proxy frames are
/// recorded (`DebuggerGates::c_lambda_frames_armed`).
pub(crate) fn note_lambda_proxy_row(shared: &SharedVm, proxy: ClassId, index: u16) {
    if shared
        .debug
        .lambda_proxy_rows
        .lock()
        .contains_key(&(proxy, index))
    {
        return;
    }
    if let Some(row) =
        crate::runtime::lambda_proxy::lambda_proxy_row(shared, proxy, usize::from(index))
    {
        shared
            .debug
            .lambda_proxy_rows
            .lock()
            .insert((proxy, index), row);
    }
}

/// The method index of lambda proxy `proxy` whose JDWP method id is
/// `method_id`, among the methods recorded calls entered
/// (`DebugRealm::lambda_proxy_rows`): how a JDWP listing's proxy row
/// (`interpreter::ListingSplice`) becomes a `jmethodID` again (the i54-L1
/// retirement).
pub(in crate::jvmti::native_env) fn proxy_method_of_jdwp(shared: &SharedVm, proxy: ClassId, method_id: u64) -> Option<u16> {
    shared
        .debug
        .lambda_proxy_rows
        .lock()
        .iter()
        .find(|((id, _), row)| {
            *id == proxy && crate::debug::jdwp_method_id(&row.name, &row.descriptor) == method_id
        })
        .map(|((_, index), _)| *index)
}

/// Local `slot` of `kind` of lambda proxy frame `record` of `thread`
/// ([`LambdaProxyFrame`]): a reference is its current (pinned) address.
/// `JVMTI_ERROR_INVALID_SLOT` past the frame's slots, `TYPE_MISMATCH` for
/// another kind, as HotSpot answers for the spun frame (MEASURED,
/// `L1K54JvmtiLambdaProxyLocalsAndFields`).
pub(in crate::jvmti::native_env) fn proxy_local_read(
    thread: &JvmThread,
    record: usize,
    slot: JInt,
    kind: LocalKind,
) -> Result<Value, JInt> {
    use crate::threading::jvm_thread::ProxyLocalSlot as S;
    let slots = &thread
        .lambda_proxy_locals
        .get(record)
        .ok_or(ERR_OPAQUE_FRAME)?
        .slots;
    let at = usize::try_from(slot)
        .ok()
        .filter(|&s| s < slots.len())
        .ok_or(ERR_INVALID_SLOT)?;
    match (kind, slots[at]) {
        (LocalKind::Int, S::Int(v)) => Ok(Value::Int(v)),
        (LocalKind::Long, S::Long(v)) => Ok(Value::Long(v)),
        (LocalKind::Float, S::Float(v)) => Ok(Value::Float(v)),
        (LocalKind::Double, S::Double(v)) => Ok(Value::Double(v)),
        (LocalKind::Object, S::Ref { pin, null }) => {
            let obj = thread
                .native_pin_roots
                .get(pin)
                .copied()
                .ok_or(ERR_OPAQUE_FRAME)?;
            Ok(Value::Object((!null).then_some(obj)))
        }
        _ => Err(ERR_TYPE_MISMATCH),
    }
}

/// Write `value` (of `kind`) into local `slot` of lambda proxy frame
/// `record` of `thread` ([`proxy_local_read`]'s errors); a reference
/// replaces the slot's pin (a null one pins the proxy, marked null).
pub(in crate::jvmti::native_env) fn proxy_local_write(
    thread: &mut JvmThread,
    record: usize,
    slot: JInt,
    kind: LocalKind,
    value: Value,
) -> Result<(), JInt> {
    use crate::threading::jvm_thread::ProxyLocalSlot as S;
    let receiver_pin = match thread
        .lambda_proxy_locals
        .get(record)
        .and_then(|r| r.slots.first())
    {
        Some(&S::Ref { pin, .. }) => pin,
        _ => return Err(ERR_OPAQUE_FRAME),
    };
    let receiver = thread
        .native_pin_roots
        .get(receiver_pin)
        .copied()
        .ok_or(ERR_OPAQUE_FRAME)?;
    let slots = &mut thread.lambda_proxy_locals[record].slots;
    let at = usize::try_from(slot)
        .ok()
        .filter(|&s| s < slots.len())
        .ok_or(ERR_INVALID_SLOT)?;
    let written = match (kind, slots[at], value) {
        (LocalKind::Int, S::Int(_), Value::Int(v)) => S::Int(v),
        (LocalKind::Long, S::Long(_), Value::Long(v)) => S::Long(v),
        (LocalKind::Float, S::Float(_), Value::Float(v)) => S::Float(v),
        (LocalKind::Double, S::Double(_), Value::Double(v)) => S::Double(v),
        (LocalKind::Object, S::Ref { pin, .. }, Value::Object(obj)) => {
            let target = thread
                .native_pin_roots
                .get_mut(pin)
                .ok_or(ERR_OPAQUE_FRAME)?;
            *target = obj.unwrap_or(receiver);
            S::Ref {
                pin,
                null: obj.is_none(),
            }
        }
        _ => return Err(ERR_TYPE_MISMATCH),
    };
    slots[at] = written;
    Ok(())
}

/// The `JvmThread::running_native` a [`CInNativeMark`] records when none is
/// recorded, so that `interpreter::NestedJavaAck` lowers the mark for Java
/// the native calls. Never a native's callback address.
pub(crate) const C_IN_NATIVE_RUNNING: usize = crate::runtime::interpreter::C_NATIVE_CALLBACK;

/// A JNI native's C code runs on this thread (interpreter round i2 wave 58,
/// lane L1; `DebuggerGates::c_in_native_armed`,
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// raises `GcBlockState::jvmti_in_native` around the JNI door's call, so
/// that [`get_thread_state`] answers `JVMTI_THREAD_STATE_IN_NATIVE` for the
/// thread, as HotSpot answers for a thread in `_thread_in_native` (MEASURED,
/// `tools/probes/interp/L1/L1W58JvmtiInNative.java`: a native spinning in C
/// `0x400005`, its Java upcall `0x5`). Without a debugger session it also
/// records [`C_IN_NATIVE_RUNNING`] as the running native when none is, so
/// that Java the native calls lowers the mark while it runs
/// (`interpreter::NestedJavaAck`); with one, the record is the debugger's
/// and is left alone. Inert, one test and one load, unless a C JVMTI env is
/// bound and the switch is on. `Drop` restores what the caller had.
pub(crate) struct CInNativeMark {
    pub(in crate::jvmti::native_env) thread: *mut JvmThread,
    pub(in crate::jvmti::native_env) engaged: bool,
    pub(in crate::jvmti::native_env) prior: bool,
    pub(in crate::jvmti::native_env) recorded: bool,
}

impl CInNativeMark {
    /// Raise the mark for `thread`'s JNI native call.
    ///
    /// # Safety
    ///
    /// `thread` is the calling thread's live `JvmThread`, and outlives the
    /// mark.
    #[inline]
    pub(crate) unsafe fn enter(shared: &SharedVm, thread: *mut JvmThread) -> Self {
        if !shared.debug.debugger_gates.c_in_native_armed() {
            return Self {
                thread,
                engaged: false,
                prior: false,
                recorded: false,
            };
        }
        // SAFETY: the caller's contract.
        unsafe { Self::enter_armed(shared, thread) }
    }

    #[cold]
    #[inline(never)]
    pub(in crate::jvmti::native_env) unsafe fn enter_armed(shared: &SharedVm, thread: *mut JvmThread) -> Self {
        // SAFETY: the caller's contract: this thread's own live `JvmThread`.
        let thread_ref = unsafe { &mut *thread };
        let prior = thread_ref
            .gc_block_state
            .jvmti_in_native
            .swap(true, std::sync::atomic::Ordering::AcqRel);
        let recorded =
            thread_ref.running_native == 0 && !shared.debug.debugger_gates.session_attached();
        if recorded {
            thread_ref.running_native = C_IN_NATIVE_RUNNING;
        }
        if crate::runtime::env_cache::frame_trace() {
            eprintln!("[JVMTI_C_IN_NATIVE] tid={} enter", thread_ref.thread_id.0);
        }
        Self {
            thread,
            engaged: true,
            prior,
            recorded,
        }
    }
}

impl Drop for CInNativeMark {
    #[inline]
    fn drop(&mut self) {
        if !self.engaged {
            return;
        }
        // SAFETY: `thread` outlives the mark (`enter`'s contract).
        let thread = unsafe { &mut *self.thread };
        thread
            .gc_block_state
            .jvmti_in_native
            .store(self.prior, std::sync::atomic::Ordering::Release);
        if self.recorded && thread.running_native == C_IN_NATIVE_RUNNING {
            thread.running_native = 0;
        }
    }
}

/// Does live thread `tid` run native code with no Java above it, as HotSpot's
/// `_thread_in_native` (wave 58; [`CInNativeMark`])? A JNI native's C code
/// under the mark, a JNI native or a GC-safe FFM downcall in the in-native
/// region (`GcBlockState::foreign_native` with `in_blocked_region`), or a
/// lazy FFM downcall. Never the calling thread: it is in the VM, in this
/// function (HotSpot MEASURED: `GetThreadState` of itself from a native
/// answers `0x5`).
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn thread_in_native(shared: &SharedVm, tid: u64) -> bool {
    let current = jni::current_jvm_thread_of(shared)
        // SAFETY: the calling thread's own, live `JvmThread`; `thread_id` is
        // an immutable `Copy` field.
        .map(|current| unsafe { (*current).thread_id.0 });
    if current == Some(tid) {
        return false;
    }
    // Wave 59 (lane L6): the registry's one in-native answer, which also
    // counts a registered native blocked in C (`BlockKind::InNative`,
    // HotSpot MEASURED `0x400005` for a thread in `ServerSocket.accept`).
    let registry = &shared.threads.thread_registry;
    if registry.thread_in_native(ThreadId(tid)) {
        return true;
    }
    // Interpreter round i2 wave 61, lane L6 (`CRATONVM_JVMTI_C_RUNNING_NATIVE`):
    // a thread RUNNING a funnel native outside a blocking region, with no
    // Java above it (its native-excuse word raised), whose callback stands in
    // for a JDK `native` method (HotSpot MEASURED `0x400005` inside
    // `Deflater.deflateBytesBytes`, `L6W61JvmtiRunningNativeState`); not one
    // shadowing a Java method (`0x5`, HotSpot runs the Java).
    if !shared.debug.debugger_gates.c_running_native_armed()
        || !registry.running_funnel_native(ThreadId(tid))
    {
        if crate::runtime::env_cache::frame_trace() {
            eprintln!(
                "[JVMTI_C_RUNNING_NATIVE] tid={tid} armed={} running_funnel_native=false word=0x{:x} blocked={}",
                shared.debug.debugger_gates.c_running_native_armed(),
                registry.native_excuse_word(ThreadId(tid)),
                registry.is_blocked(ThreadId(tid)),
            );
        }
        return false;
    }
    // Wave 62 (lane L6, `CRATONVM_HANDSHAKE_NATIVE_CALLBACK`): the callback
    // the raised excuse word itself records, when it does; the debugger
    // gate's record otherwise.
    let callback = registry
        .running_funnel_callback(ThreadId(tid))
        .or_else(|| registry.funnel_native_callback(ThreadId(tid)));
    let native = callback.is_some_and(|cb| {
        crate::runtime::interpreter::funnel_callback_runs_a_native_method(shared, cb)
    });
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_C_RUNNING_NATIVE] tid={tid} callback=0x{:x} native={native}",
            callback.unwrap_or(0)
        );
    }
    native
}

/// The VM a stack function acts on, when it is in the live phase (the stack
/// functions are live-phase only).
pub(in crate::jvmti::native_env) fn live_phase_vm(env: *mut JvmtiNativeEnv) -> Result<Arc<SharedVm>, JInt> {
    // gci i16/sn (`CRATONVM_DEBUG_SUSPEND_AT_NATIVE_TRANSITION`): the C
    // functions behind this prologue -- the thread, stack, suspension and
    // agent-thread functions -- are VM entries in HotSpot (`JVMTI_ENTRY`,
    // `ThreadInVMfromNative`), so a suspended caller whose C code runs in
    // native stops here as at a JNIEnv function, before it reads anything.
    // Inert unless this thread's innermost JNI native call is in native.
    jni::hold_in_native_while_suspended();
    let env = live_env(env)?;
    if env.outside_live_phase() {
        return Err(ERR_WRONG_PHASE);
    }
    env.vm().ok_or(ERR_WRONG_PHASE)
}

/// Slot 18: `GetCurrentThread(env, thread_ptr)` (wave 22): the calling
/// thread's `java.lang.Thread` as a local reference, NULL while it has none;
/// `JVMTI_ERROR_UNATTACHED_THREAD` for a thread the VM does not know.
pub(in crate::jvmti::native_env) extern "C" fn get_current_thread(env: *mut JvmtiNativeEnv, thread_ptr: *mut JObject) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    if thread_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
    let tid = unsafe { (*current).thread_id };
    // The mirror is read and made a handle inside the foreign-thread entry,
    // as `thread_id_of` reads one, and in `shared`'s JNI context.
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(&shared);
    let handle = shared
        .threads
        .thread_registry
        .java_thread_obj(tid)
        .map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *thread_ptr = handle };
    ERR_NONE
}

/// Slot 16: `GetFrameCount(env, thread, count_ptr)` (wave 22; the current
/// thread, [`frames_of`]).
pub(in crate::jvmti::native_env) extern "C" fn get_frame_count(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    count_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let frames = match frames_of(&shared, thread) {
        Ok(frames) => frames,
        Err(code) => return code,
    };
    let Ok(count) = JInt::try_from(frames.len()) else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *count_ptr = count };
    ERR_NONE
}

/// Slot 19: `GetFrameLocation(env, thread, depth, method_ptr, location_ptr)`
/// (wave 22; the current thread, [`frames_of`]): depth 0 is the top frame;
/// `JVMTI_ERROR_NO_MORE_FRAMES` past the bottom.
pub(in crate::jvmti::native_env) extern "C" fn get_frame_location(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    method_ptr: *mut u64,
    location_ptr: *mut i64,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(depth) = usize::try_from(depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if method_ptr.is_null() || location_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let frames = match frames_of(&shared, thread) {
        Ok(frames) => frames,
        Err(code) => return code,
    };
    let Some(frame) = frames.get(depth) else {
        return ERR_NO_MORE_FRAMES;
    };
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *method_ptr = frame.method;
        *location_ptr = frame.location;
    }
    ERR_NONE
}

/// Slot 104: `GetStackTrace(env, thread, start_depth, max_frame_count,
/// frame_buffer, count_ptr)` (wave 22; the current thread, [`frames_of`]).
/// A non-negative `start_depth` counts from the top, a negative one from the
/// bottom (`-n`: the lowest `n` frames); frames are written from there toward
/// the bottom, at most `max_frame_count`. `JVMTI_ERROR_ILLEGAL_ARGUMENT` for
/// a positive `start_depth` at or past the depth, a negative one below
/// `-depth`, or a negative `max_frame_count`, as the specification lists.
pub(in crate::jvmti::native_env) extern "C" fn get_stack_trace(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    start_depth: JInt,
    max_frame_count: JInt,
    frame_buffer: *mut FrameInfo,
    count_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(max) = usize::try_from(max_frame_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if frame_buffer.is_null() || count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let frames = match frames_of(&shared, thread) {
        Ok(frames) => frames,
        Err(code) => return code,
    };
    let Some(start) = stack_trace_start(frames.len(), start_depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let taken = &frames[start..];
    let taken = &taken[..taken.len().min(max)];
    let Ok(count) = JInt::try_from(taken.len()) else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's buffer holds at least `max_frame_count` entries
    // (the function's contract), and `taken.len()` is at most that; the
    // out-parameters are non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(taken.as_ptr(), frame_buffer, taken.len());
        *count_ptr = count;
    }
    ERR_NONE
}

/// The index (into the frames, top first) `GetStackTrace` starts at for
/// `start_depth` over a stack of `depth` frames, or `None` for the
/// specification's `ILLEGAL_ARGUMENT` cases.
pub(in crate::jvmti::native_env) fn stack_trace_start(depth: usize, start_depth: JInt) -> Option<usize> {
    if start_depth >= 0 {
        let start = usize::try_from(start_depth).ok()?;
        // "positive and greater than or equal to stackDepth": a zero start
        // over an empty stack answers no frames.
        (start == 0 || start < depth).then_some(start)
    } else {
        let from_bottom = usize::try_from(start_depth.unsigned_abs()).ok()?;
        depth.checked_sub(from_bottom)
    }
}

// ---------------------------------------------------------------------------
// Every thread's stack (interpreter round i1 wave 46, lane L1)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`:
// sampling profilers and thread-dump agents enumerate threads with
// `GetAllThreads` and read every stack with one `GetAllStackTraces` or
// `GetThreadListStackTraces`, which answered `JVMTI_ERROR_NOT_AVAILABLE`, so
// such an agent saw no stack at all on this VM. Each stack is read as
// `GetStackTrace` reads it ([`frames_of`]): the calling thread's own, another
// through its window, its park or a handshake of its own. HotSpot reads
// every thread at one safepoint, so its stacks are consistent with each
// other; these are read one after another. Codes as HotSpot 25.0.3's,
// measured (`tools/probes/interp/L1/L1W46JvmtiAllStackTraces.java`).

/// `jvmtiStackInfo`: `{ jthread thread; jint state; jvmtiFrameInfo
/// *frame_buffer; jint frame_count; }`, 32 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy)]
pub(in crate::jvmti::native_env) struct StackInfoRow {
    pub(in crate::jvmti::native_env) thread: JObject,
    pub(in crate::jvmti::native_env) state: JInt,
    pub(in crate::jvmti::native_env) frame_buffer: *mut FrameInfo,
    pub(in crate::jvmti::native_env) frame_count: JInt,
}

/// The live PLATFORM threads' `java.lang.Thread` objects, in the order they
/// registered (interpreter round i5 wave 102, lane L1). JVMTI's
/// `GetAllThreads`, `GetAllStackTraces` and `GetThreadGroupChildren` return
/// "live platform threads"; virtual threads are not returned by them (JDK 21+
/// JVMTI specification; HotSpot walks its threads list, where a virtual
/// thread is never an entry). This VM registers each virtual thread as an
/// entry of its own whose mirror is the `VirtualThread`, so every one of them
/// was listed, in the registry map's hash order, which also changed from run
/// to run where HotSpot's list keeps attach order.
pub(in crate::jvmti::native_env) fn live_platform_thread_objects(shared: &SharedVm) -> Vec<ObjectRef> {
    let mut threads = shared
        .threads
        .thread_registry
        .alive_thread_objects_in_id_order();
    threads.retain(|&t| !extensions::is_virtual_thread_object(shared, t));
    threads
}

/// Every live thread of `shared` that has a `java.lang.Thread`, as local
/// references of the calling thread (made inside its foreign-thread entry,
/// in `shared`'s JNI context).
pub(in crate::jvmti::native_env) fn all_thread_handles(shared: &SharedVm) -> Vec<JObject> {
    let _fx = jni::ForeignJniEntry::enter();
    // Interpreter round i5 wave 102, lane L1: platform threads only, in
    // attach order (see [`live_platform_thread_objects`]).
    let threads = live_platform_thread_objects(shared);
    let prev = jni::replace_jni_context(shared);
    let handles = threads.into_iter().map(jni::new_local_handle).collect();
    jni::restore_jni_context(prev);
    handles
}

/// Slot 4: `GetAllThreads(env, threads_count_ptr, threads_ptr)` (wave 46;
/// no capability): the VM's live threads, in no particular order, as local
/// references in an `Allocate`d array.
pub(in crate::jvmti::native_env) extern "C" fn get_all_threads(
    env: *mut JvmtiNativeEnv,
    threads_count_ptr: *mut JInt,
    threads_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if threads_count_ptr.is_null() || threads_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let handles = all_thread_handles(&shared);
    let Ok(count) = JInt::try_from(handles.len()) else {
        return ERR_INTERNAL;
    };
    let Some(size) = handles.len().checked_mul(std::mem::size_of::<JObject>()) else {
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<JObject>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `block` holds `handles.len()` handles (`allocate_block` of
    // their size, 16-aligned); the out-parameters are the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(handles.as_ptr(), block, handles.len());
        *threads_count_ptr = count;
        *threads_ptr = block;
    }
    ERR_NONE
}

/// One thread's `jvmtiStackInfo` before it is written out: the handle, the
/// `GetThreadState` answer and at most `max` frames, top first.
pub(in crate::jvmti::native_env) struct StackOf {
    pub(in crate::jvmti::native_env) thread: JObject,
    pub(in crate::jvmti::native_env) state: JInt,
    pub(in crate::jvmti::native_env) frames: Vec<FrameInfo>,
}

/// [`StackOf`] for thread `handle`: a thread never started is state 0 with
/// no frames, a terminated one (or one that ends while it is read)
/// `TERMINATED` with none, as HotSpot 25.0.3 answers them;
/// `JVMTI_ERROR_INVALID_THREAD` for a handle that names no thread.
pub(in crate::jvmti::native_env) fn stack_of(shared: &SharedVm, handle: JObject, max: usize) -> Result<StackOf, JInt> {
    let (state, mut frames) = match lookup_thread(shared, handle)? {
        ThreadLookup::Unstarted => (0, Vec::new()),
        ThreadLookup::Dead => (THREAD_STATE_TERMINATED, Vec::new()),
        ThreadLookup::Live(tid) => {
            let frames = match frames_of(shared, handle) {
                Ok(frames) => frames,
                Err(ERR_THREAD_NOT_ALIVE) => Vec::new(),
                // A thread that did not stand still in time (a JNI native
                // running outside a blocking region, a compiled loop with
                // no exit-capable poll): listed with no frames rather than
                // failing every other thread's stack. HotSpot's handshake
                // reaches such a thread.
                Err(ERR_NOT_AVAILABLE) => {
                    if crate::runtime::env_cache::frame_trace() {
                        eprintln!("[JVMTI_ALL_STACKS] tid={tid} not stopped: no frames");
                    }
                    Vec::new()
                }
                Err(code) => return Err(code),
            };
            let state = if shared.threads.thread_registry.is_alive(ThreadId(tid)) {
                live_thread_state(shared, tid)
            } else {
                THREAD_STATE_TERMINATED
            };
            (state, frames)
        }
    };
    frames.truncate(max);
    Ok(StackOf {
        thread: handle,
        state,
        frames,
    })
}

/// `stacks` as one `Allocate`d block: the `jvmtiStackInfo` array, then every
/// frame buffer after it ("the frame buffers are allocated in the same
/// block"); null when out of memory.
pub(in crate::jvmti::native_env) fn allocate_stack_infos(stacks: &[StackOf]) -> *mut StackInfoRow {
    let frames: usize = stacks.iter().map(|s| s.frames.len()).sum();
    let size = stacks
        .len()
        .checked_mul(std::mem::size_of::<StackInfoRow>())
        .and_then(|head| {
            frames
                .checked_mul(std::mem::size_of::<FrameInfo>())
                .and_then(|tail| head.checked_add(tail))
        });
    let Some(size) = size else {
        return std::ptr::null_mut();
    };
    let block = allocate_block(size);
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let rows = block.cast::<StackInfoRow>();
    // SAFETY: the frame buffers start right after the rows, inside the
    // block, 8-aligned (a row is 32 bytes and the block 16-aligned).
    let mut buffer = unsafe { block.add(stacks.len() * std::mem::size_of::<StackInfoRow>()) }
        .cast::<FrameInfo>();
    for (i, stack) in stacks.iter().enumerate() {
        let count = JInt::try_from(stack.frames.len()).unwrap_or(JInt::MAX);
        // SAFETY: `block` holds `stacks.len()` rows and every stack's frames
        // after them (`size`); `buffer` stays inside it.
        unsafe {
            std::ptr::copy_nonoverlapping(stack.frames.as_ptr(), buffer, stack.frames.len());
            rows.add(i).write(StackInfoRow {
                thread: stack.thread,
                state: stack.state,
                frame_buffer: buffer,
                frame_count: count,
            });
            buffer = buffer.add(stack.frames.len());
        }
    }
    rows
}

/// Slot 101: `GetThreadListStackTraces(env, thread_count, thread_list,
/// max_frame_count, stack_info_ptr)` (wave 46; no capability): each listed
/// thread's state and at most `max_frame_count` frames, in the list's
/// order. The checks in HotSpot 25.0.3's order: a negative count
/// (`ILLEGAL_ARGUMENT`), a NULL list (`NULL_POINTER`), a negative
/// `max_frame_count` (`ILLEGAL_ARGUMENT`), a NULL `stack_info_ptr`
/// (`NULL_POINTER`), then a handle naming no thread (`INVALID_THREAD`).
pub(in crate::jvmti::native_env) extern "C" fn get_thread_list_stack_traces(
    env: *mut JvmtiNativeEnv,
    thread_count: JInt,
    thread_list: *const JObject,
    max_frame_count: JInt,
    stack_info_ptr: *mut *mut StackInfoRow,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(count) = usize::try_from(thread_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if thread_list.is_null() {
        return ERR_NULL_POINTER;
    }
    let Ok(max) = usize::try_from(max_frame_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if stack_info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's list holds `thread_count` handles (the function's
    // contract), and it is non-null.
    let handles = unsafe { std::slice::from_raw_parts(thread_list, count) }.to_vec();
    let mut stacks = Vec::with_capacity(handles.len());
    for handle in handles {
        match stack_of(&shared, handle, max) {
            Ok(stack) => stacks.push(stack),
            Err(code) => return code,
        }
    }
    let block = allocate_stack_infos(&stacks);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *stack_info_ptr = block };
    ERR_NONE
}

/// Slot 100: `GetAllStackTraces(env, max_frame_count, stack_info_ptr,
/// thread_count_ptr)` (wave 46; no capability): [`get_thread_list_stack_traces`]
/// over [`get_all_threads`]' threads; a thread that ends while the others
/// are read is left out.
pub(in crate::jvmti::native_env) extern "C" fn get_all_stack_traces(
    env: *mut JvmtiNativeEnv,
    max_frame_count: JInt,
    stack_info_ptr: *mut *mut StackInfoRow,
    thread_count_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Ok(max) = usize::try_from(max_frame_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if stack_info_ptr.is_null() || thread_count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let mut stacks = Vec::new();
    for handle in all_thread_handles(&shared) {
        match stack_of(&shared, handle, max) {
            Ok(stack) if stack.state & THREAD_STATE_ALIVE != 0 => stacks.push(stack),
            // Ended meanwhile (or not a thread any more): not listed.
            Ok(_) | Err(ERR_THREAD_NOT_ALIVE | ERR_INVALID_THREAD) => {}
            Err(code) => return code,
        }
    }
    let Ok(count) = JInt::try_from(stacks.len()) else {
        return ERR_INTERNAL;
    };
    let block = allocate_stack_infos(&stacks);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_ALL_STACKS] threads={count}");
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *stack_info_ptr = block;
        *thread_count_ptr = count;
    }
    ERR_NONE
}

// ---------------------------------------------------------------------------
// Other threads: suspension, thread state and stacks (interpreter round i1
// wave 45, lane L1)
// ---------------------------------------------------------------------------
//
// Stage 1 of
// `docs/feature-designs/jvmti-delivery-threading.md`.
// A C agent's `SuspendThread` is the JDWP suspension's park
// (`DebugState::jvmti_suspend`: the thread stops at its next interpreter
// suspend point, or at the return of the native it runs, and a thread blocked
// in a native region stays there), with JVMTI's flag semantics. The stack
// functions read another thread where it stands still: blocked, through its
// inspection window (`interpreter::blocked_frame_rows`); parked, from the
// listing its park published (`DebugState::thread_frames`); running, after a
// handshake, a suspension of its own for the read (`DebugState::begin_handshake`).
// Each code is HotSpot 25.0.3's, measured
// (`tools/probes/interp/L1/L1W45JvmtiSuspendAndOtherStacks.java`).

/// How long a `SuspendThread` or a stack read waits for another thread to
/// stop: a thread in a compiled loop stops only at an exit-capable poll
/// (`interpreter::request_compiled_loop_exits`). A suspension that is not
/// reached by then still stands; a stack read answers
/// `JVMTI_ERROR_NOT_AVAILABLE`.
pub(in crate::jvmti::native_env) const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Wait, polling every millisecond, until `done` answers `true` (then
/// `true`) or `deadline` passes (`false`). The calling thread waits GC-safe:
/// a registered mutator (a JNI native, an event callback) waits inside a
/// blocking region, as a native's `Thread.sleep` does, so a collection that
/// another thread (the one it waits for, perhaps) starts meanwhile does not
/// wait for it. A thread already GC-blocked (an attached agent thread idle
/// between calls) or unknown to the VM just sleeps.
pub(in crate::jvmti::native_env) fn wait_gc_safe(
    shared: &SharedVm,
    deadline: Option<std::time::Instant>,
    mut done: impl FnMut() -> bool,
) -> bool {
    if done() {
        return true;
    }
    gc_safe(shared, || loop {
        if done() {
            break true;
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break false;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    })
}

/// Run `work`, which waits for another thread, with the calling thread
/// GC-safe (see [`wait_gc_safe`]).
pub(in crate::jvmti::native_env) fn gc_safe<R>(shared: &SharedVm, work: impl FnOnce() -> R) -> R {
    use cratonvm_native_api::NativeThreadAccess as _;
    let current = jni::current_jvm_thread_of(shared).filter(|&thread| {
        // SAFETY: the calling thread's own, live `JvmThread`.
        let blocked = unsafe { &*thread }
            .gc_block_state
            .in_blocked_region
            .load(std::sync::atomic::Ordering::Acquire);
        !blocked
    });
    let mut region = current.map(|thread| crate::vm::vm_exec::NativeContextImpl {
        shared,
        // SAFETY: the calling thread's own, live `JvmThread`, which this call
        // runs on and does not otherwise touch until the region ends.
        thread: unsafe { &mut *thread },
    });
    if let Some(ctx) = region.as_mut() {
        // Interpreter round i2 wave 66, lane L1
        // (`CRATONVM_VM_INTERNAL_WAITS_RUNNABLE`): HotSpot waits for the
        // target inside the VM (a handshake, `_thread_blocked` in a JVM
        // entry), so the caller reads RUNNABLE, not a WAITING region with no
        // wait kind.
        if shared.threads.thread_registry.vm_internal_waits_runnable() {
            ctx.begin_vm_wait_region();
        } else {
            ctx.begin_blocking_region();
        }
    }
    let answer = work();
    if let Some(ctx) = region.as_mut() {
        ctx.end_blocking_region();
    }
    answer
}

/// Has thread `tid` stopped for a suspension: parked at a suspend point,
/// blocked in a native region, or gone?
pub(in crate::jvmti::native_env) fn thread_stopped(shared: &SharedVm, tid: u64) -> bool {
    let registry = &shared.threads.thread_registry;
    let id = ThreadId(tid);
    !registry.is_alive(id)
        || registry.is_blocked(id)
        // An unmounted virtual thread runs nothing until it is mounted again,
        // which honours the suspension (interpreter round i1 wave 50, lane L1).
        || shared.threads.virtual_thread_manager.is_deposited(tid)
        // Interpreter round i5 wave 101, lane L1: in a native outside a
        // region with no Java above it, the acknowledgement a JDWP
        // suspension's settle wait already counts stopped (raised only while
        // a JDWP session is attached, `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`);
        // the native's return parks it, named or not. HotSpot's handshake
        // counts a thread in native safe at once; this wait ran to its 2 s
        // bound for one.
        || (shared.debug.debugger_gates.suspend_native_ack()
            && crate::debug::in_native_acknowledged(registry, id))
        || shared.debug.debug_state.lock().is_parked(tid)
}

/// The listing of thread `tid` while it stands still, top first: parked,
/// the listing its park published in this suspension; blocked, read through
/// its window.
pub(in crate::jvmti::native_env) fn stopped_rows(shared: &SharedVm, tid: u64) -> Option<Vec<BlockedRow>> {
    {
        let ds = shared.debug.debug_state.lock();
        if ds.is_parked(tid) {
            if let Some(rows) = published_rows(&ds, tid, None) {
                return Some(rows);
            }
        }
    }
    crate::runtime::interpreter::with_blocked_frames(shared, tid, |held| held.rows.clone())
}

/// The listing parked thread `tid` published for its current suspension
/// (`DebugState::thread_frames`), top first, as [`BlockedRow`]s (interpreter
/// round i1 wave 46, lane L1): with `depth`, the thread's interpreter frame
/// count (known on the thread itself), each interpreter row names its frame,
/// counted from the bottom as `debug::inspect`'s `set_frame_values` counts
/// it; the native method heading the listing and a compiled activation name
/// none. `None` when no listing of this suspension stands. Also JDWP's
/// OSR'd-frame home read (`compiled_frame_locals::publish_osr_home_locals`,
/// j27 wave 8, lane W8INTR).
pub(crate) fn published_rows(
    ds: &crate::debug::DebugState,
    tid: u64,
    depth: Option<usize>,
) -> Option<Vec<BlockedRow>> {
    if !ds.has_current_frames(tid) {
        return None;
    }
    let entries = ds.thread_frames.get(&tid)?;
    let mut from_top = 0usize;
    let rows = entries
        .iter()
        .map(|e| {
            let native = e.offset == crate::debug::NATIVE_FRAME_LOCATION;
            // `true`: a compiled activation; `false`: an interpreter frame
            // whose body runs compiled.
            let opaque = ds.opaque_frames.get(&(tid, e.frame_id)).copied();
            // Interpreter round i7 wave 138, lane L1: a lambda proxy
            // method's row names no interpreter frame
            // (`DebugState::proxy_frames`); counting it shifted every row
            // below it onto the frame under its own.
            let interpreted = !native
                && opaque != Some(true)
                && !ds.proxy_frames.contains(&(tid, e.frame_id));
            let frame = if interpreted {
                let index = depth.and_then(|d| d.checked_sub(from_top + 1));
                from_top += 1;
                index
            } else {
                None
            };
            BlockedRow {
                class_id: e.class_id,
                method_id: e.method_id,
                location: if native {
                    -1
                } else {
                    i64::try_from(e.offset).unwrap_or(-1)
                },
                frame,
                runs_compiled: opaque == Some(false),
                // A published listing keeps no chain index (another thread's:
                // its owned-monitor depths attribute by frame).
                chain: None,
            }
        })
        .collect();
    Some(rows)
}

/// A row of another thread's listing as the C table lists it (interpreter
/// round i1 wave 46, lane L1): its `jvmtiFrameInfo`, and the raw row it came
/// from (the interpreter frame it is, to read its locals and monitors).
#[derive(Clone, Copy)]
pub(in crate::jvmti::native_env) struct OtherRow {
    pub(in crate::jvmti::native_env) info: FrameInfo,
    pub(in crate::jvmti::native_env) raw: BlockedRow,
}

/// `rows` (top first) as the C table lists them: a row whose method no
/// longer resolves in its class is left out (an obsolete method's null JDWP
/// id names none; it has no `jmethodID`), so a depth counts the rows
/// [`frames_of`] answers for the thread.
pub(in crate::jvmti::native_env) fn other_rows(shared: &SharedVm, rows: Vec<BlockedRow>) -> Vec<OtherRow> {
    let cm = shared.classes.class_manager.read();
    let mut out = Vec::with_capacity(rows.len());
    for raw in rows {
        let Ok(class_raw) = u32::try_from(raw.class_id) else {
            continue;
        };
        let Some(class) = cm.get_class(ClassId::new(class_raw)) else {
            // The i54-L1 retirement: a lambda proxy method's row.
            if let Some(index) =
                proxy_method_of_jdwp(shared, ClassId::new(class_raw), raw.method_id)
            {
                out.push(OtherRow {
                    info: FrameInfo {
                        // Widening: wave 56's `(proxy id << 32) | index`.
                        method: (u64::from(class_raw) << 32) | u64::from(index),
                        location: raw.location,                    },
                    raw,                });            }
            continue;
        };
        // The JDWP method id is the row hash (`native_row_method_hash`).
        let Some(index) = row_method_index(shared, class, raw.method_id | ROW_KEY_IS_HASH)
            .and_then(|i| u16::try_from(i).ok())
        else {
            continue;
        };
        out.push(OtherRow {
            info: FrameInfo {
                // Widening: u32 class id and u16 index into their halves of the id.
                method: (u64::from(class_raw) << 32) | u64::from(index),
                location: raw.location,            },
            raw,        });    }
    out
}

/// `rows` (top first, [`other_rows`] of the calling thread's shared listing)
/// with a lambda proxy row between each call site and the lambda
/// implementation it reached through a proxy no row recorded, each answered
/// with whether it was inferred (interpreter round i6 wave 120 follow-up,
/// lane L1): the i54-L1 retirement's [`with_inferred_proxy_rows`] on the
/// rows of the builder JDWP uses, for `CRATONVM_JVMTI_C_LISTING_SHARED`
/// while `DebuggerGates::proxy_rows_incomplete` holds. The same inference
/// (`stackwalker::lambda_proxy_standin_frames`, fail-closed) over each pair
/// of adjacent rows that name a Java method at a code index (an interpreter
/// frame, a compiled or inlined activation, a spliced JDK frame); a native
/// row (-1) or a recorded proxy row stands between a call and its target
/// already. The inferred row is the proxy's functional method (index 1, wave
/// 56's scheme) at its forwarding call, with no locals record, as
/// [`listed_rows`]'s own path lists it.
pub(in crate::jvmti::native_env) fn with_inferred_proxy_other_rows(shared: &SharedVm, rows: Vec<OtherRow>) -> Vec<(OtherRow, bool)> {
    use crate::native::registry::StackTraceEntry;
    // Each row as a stack-trace entry, when it names a Java method at an
    // index: what the inference reads (the caller's invoke, the callee's
    // method).
    let entries: Vec<Option<StackTraceEntry>> = {
        let cm = shared.classes.class_manager.read();
        rows.iter()
            .map(|row| {
                let raw = &row.raw;
                let class_id = ClassId::new(u32::try_from(raw.class_id).ok()?);
                let bci = i32::try_from(raw.location).ok().filter(|&bci| bci >= 0)?;
                let class = cm.get_class(class_id)?;
                let (index, method) = class.methods.iter().enumerate().find(|(_, m)| {
                    crate::debug::jdwp_method_id(&m.name, &m.descriptor) == raw.method_id
                })?;
                Some(StackTraceEntry {
                    class_name: Arc::clone(&class.name),
                    method_name: Arc::clone(&method.name),
                    method_descriptor: Some(Arc::clone(&method.descriptor)),
                    source_file: None,
                    line_number: -1,
                    byte_code_index: bci,
                    class_id: Some(class_id),
                    method_index: u32::try_from(index).ok(),                })            })
            .collect()
    };
    // Named before the class-manager lock below (the proxies' host names).
    let trace: Vec<StackTraceEntry> = entries.iter().flatten().cloned().collect();
    let candidates = inferred_proxy_candidates(shared, &trace);
    if candidates.is_empty() {
        return rows.into_iter().map(|row| (row, false)).collect();
    }
    let cm = shared.classes.class_manager.read();
    let mut out = Vec::with_capacity(rows.len() + 2);
    for (i, row) in rows.into_iter().enumerate() {
        // Top first: row `i - 1` is the callee of row `i`.
        let callee = i.checked_sub(1).and_then(|c| entries[c].as_ref());
        if let (Some(callee), Some(caller)) = (callee, entries[i].as_ref()) {
            let pair = [callee.clone(), caller.clone()];
            let inferred = crate::runtime::stackwalker::lambda_proxy_standin_frames(
                &cm.class_store,
                &pair,
                &candidates,
            );
            if let Some(entry) = inferred.first().and_then(|(_, entries)| entries.first()) {
                if let Some(proxy) = entry.class_id {
                    if crate::runtime::env_cache::frame_trace() {
                        eprintln!(
                            "[JVMTI_C_LAMBDA_FRAMES] shared inferred class={:#x} location={}",
                            proxy.as_u32(),
                            entry.byte_code_index
                        );
                    }
                    let location = i64::from(entry.byte_code_index);
                    out.push((
                        OtherRow {
                            info: FrameInfo {
                                // Widening: the functional method, index 1
                                // (wave 56's scheme).
                                method: (u64::from(proxy.as_u32()) << 32) | 1,
                                location,
                            },
                            raw: BlockedRow {
                                class_id: u64::from(proxy.as_u32()),
                                method_id: crate::debug::jdwp_method_id(
                                    &entry.method_name,
                                    entry.method_descriptor.as_deref().unwrap_or(""),
                                ),
                                location,
                                frame: None,
                                runs_compiled: false,
                                chain: None,
                            },
                        },
                        true,
                    ));                }            }        }
        out.push((row, false));    }
    out
}

/// [`frames_of`] for thread `tid`, another than the caller: blocked, read
/// through its window at once; suspended, once it has stopped; running,
/// after a handshake. `JVMTI_ERROR_THREAD_NOT_ALIVE` when it ended meanwhile,
/// `JVMTI_ERROR_NOT_AVAILABLE` when it did not stop in [`STOP_WAIT`].
///
/// Wave 46 (lane L1): a thread parked at a suspend point lists itself, on
/// its own thread, as the current thread's frames are listed
/// ([`listed_rows`]), so a JNI native in the middle of its stack (one that
/// called back into Java) is listed as HotSpot lists it; the listing its
/// park published for JDWP names only the native on top. That listing is
/// still the answer when the thread cannot take the work (a debugger
/// invocation runs on it).
pub(in crate::jvmti::native_env) fn other_thread_frames(shared: &SharedVm, tid: u64) -> Result<Vec<FrameInfo>, JInt> {
    let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
        other_rows(shared, held.rows.clone())
            .into_iter()
            .map(|row| row.info)
            .collect::<Vec<FrameInfo>>()
    };
    // An unmounted virtual thread (a continuation that yielded in a sleep or a
    // park) is in no blocking region and no carrier runs it, so a handshake
    // would wait for it in vain: its deposited frames are read where they lie
    // (interpreter round i1 wave 50, lane L1;
    // `interpreter-L1-the-c-jvmti-table-lists-no-frames-for-an-unmounted-virtual-thread-FIXED`).
    let unmounted = || {
        crate::runtime::interpreter::unmounted_virtual_thread_rows(shared, tid).map(|rows| {
            other_rows(shared, rows)
                .into_iter()
                .map(|row| row.info)
                .collect()
        })
    };
    let blocked = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window);
    let (answer, via) = match blocked
        .map(|rows| (rows, "window"))
        .or_else(|| unmounted().map(|rows: Vec<FrameInfo>| (rows, "unmounted")))
    {
        Some((rows, via)) => (Ok(rows), via),
        None => {
            let handshake = !shared.debug.debug_state.lock().is_thread_suspended(tid);
            if handshake {
                let mut ds = shared.debug.debug_state.lock();
                ds.begin_handshake(tid);
                crate::debug::publish_debugger_gates(shared, &ds);
            }
            let answer = on_stopped_thread(
                shared,
                tid,
                |shared: &SharedVm, thread: &mut JvmThread| {
                    listed_rows(shared, thread, false)
                        .into_iter()
                        .map(|row| row.info)
                        .collect::<Vec<FrameInfo>>()
                },
                window,
            );
            if handshake {
                let mut ds = shared.debug.debug_state.lock();
                ds.end_handshake(tid);
                crate::debug::publish_debugger_gates(shared, &ds);
                if crate::runtime::env_cache::frame_trace() {
                    eprintln!(
                        "[JVMTI_SUSPEND] handshake tid={tid} stopped={}",
                        answer.is_ok()
                    );
                }
            }
            match answer {
                Ok((rows, via)) => {
                    let via = match (handshake, via) {
                        (true, _) => "handshake",
                        (false, "parked") => "suspended",
                        (false, via) => via,
                    };
                    (Ok(rows), via)
                }
                Err(ERR_NOT_AVAILABLE) => match stopped_rows(shared, tid) {
                    Some(rows) => (
                        Ok(other_rows(shared, rows)
                            .into_iter()
                            .map(|row| row.info)
                            .collect()),
                        "published",
                    ),
                    None => (Err(ERR_NOT_AVAILABLE), "none"),
                },
                Err(code) => (Err(code), "none"),
            }
        }
    };
    let out = answer?;
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_OTHER_STACK] tid={tid} rows={} via={via}", out.len());
    }
    Ok(out)
}

/// The thread a suspension function names: `(thread id, is it the calling
/// thread)`; a NULL `thread` is the calling thread.
pub(in crate::jvmti::native_env) fn suspension_target(shared: &SharedVm, thread: JObject) -> Result<(u64, bool), JInt> {
    let current_tid = jni::current_jvm_thread_of(shared)
        // SAFETY: the calling thread's own, live `JvmThread`; `thread_id` is
        // an immutable `Copy` field.
        .map(|current| unsafe { (*current).thread_id.0 });
    if thread == 0 {
        return current_tid
            .map(|tid| (tid, true))
            .ok_or(ERR_UNATTACHED_THREAD);
    }
    let tid = thread_id_of(shared, thread)?;
    Ok((tid, current_tid == Some(tid)))
}

/// Mark thread `tid` suspended by the agent and republish the debugger
/// gates, so it stops at its next suspend point.
/// `JVMTI_ERROR_THREAD_SUSPENDED` when it already is.
pub(in crate::jvmti::native_env) fn mark_suspended(shared: &SharedVm, tid: u64) -> JInt {
    let mut ds = shared.debug.debug_state.lock();
    if !ds.jvmti_suspend(tid) {
        return ERR_THREAD_SUSPENDED;
    }
    crate::debug::publish_debugger_gates(shared, &ds);
    ERR_NONE
}

/// Wait for thread `tid`, which [`mark_suspended`] suspended, to stop; the
/// calling thread itself waits until it is resumed, as JVMTI's
/// `SuspendThread` of the current thread does not return before that.
pub(in crate::jvmti::native_env) fn await_suspended(shared: &SharedVm, tid: u64, is_current: bool) {
    if is_current {
        wait_gc_safe(shared, None, || {
            !shared.debug.debug_state.lock().is_jvmti_suspended(tid)
        });
        return;
    }
    let deadline = std::time::Instant::now() + STOP_WAIT;
    let stopped = wait_gc_safe(shared, Some(deadline), || thread_stopped(shared, tid));
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[JVMTI_SUSPEND] suspend tid={tid} stopped={stopped}");
    }
}

/// End the agent's suspension of thread `tid`.
/// `JVMTI_ERROR_THREAD_NOT_SUSPENDED` when it has none.
pub(in crate::jvmti::native_env) fn mark_resumed(shared: &SharedVm, tid: u64) -> JInt {
    let mut ds = shared.debug.debug_state.lock();
    if !ds.jvmti_resume(tid) {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    crate::debug::publish_debugger_gates(shared, &ds);
    ERR_NONE
}

/// Slot 5: `SuspendThread(env, thread)` (wave 45; `can_suspend`). Returns
/// once the thread has stopped (or after [`STOP_WAIT`], still suspended); the
/// calling thread returns only once resumed.
pub(in crate::jvmti::native_env) extern "C" fn suspend_thread(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_SUSPEND) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let (tid, is_current) = match suspension_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let code = mark_suspended(&shared, tid);
    if code == ERR_NONE {
        await_suspended(&shared, tid, is_current);
    }
    code
}

/// Slot 6: `ResumeThread(env, thread)` (wave 45; `can_suspend`).
pub(in crate::jvmti::native_env) extern "C" fn resume_thread(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_SUSPEND) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    match suspension_target(&shared, thread) {
        Ok((tid, _)) => mark_resumed(&shared, tid),
        Err(code) => code,
    }
}

/// The checks `SuspendThreadList` and `ResumeThreadList` make before any
/// thread (wave 45): the capability, a negative count
/// (`ILLEGAL_ARGUMENT`), a NULL list or results array (`NULL_POINTER`).
/// Answers the VM and the list's handles.
pub(in crate::jvmti::native_env) fn thread_list(
    env: *mut JvmtiNativeEnv,
    request_count: JInt,
    request_list: *const JObject,
    results: *mut JInt,
) -> Result<(Arc<SharedVm>, Vec<JObject>), JInt> {
    let shared = live_phase_vm_with(env, 0, CAN_SUSPEND)?;
    let count = usize::try_from(request_count).map_err(|_| ERR_ILLEGAL_ARGUMENT)?;
    if request_list.is_null() || results.is_null() {
        return Err(ERR_NULL_POINTER);
    }
    // SAFETY: the agent's list holds `request_count` handles (the function's
    // contract), and it is non-null.
    let handles = unsafe { std::slice::from_raw_parts(request_list, count) }.to_vec();
    Ok((shared, handles))
}

/// Slot 92: `SuspendThreadList(env, request_count, request_list, results)`
/// (wave 45; `can_suspend`): each thread's own answer in `results` (a thread
/// listed twice is `THREAD_SUSPENDED` the second time), `JVMTI_ERROR_NONE`
/// overall. Every other thread is marked first, then waited for; the calling
/// thread, if listed, waits last, until it is resumed.
pub(in crate::jvmti::native_env) extern "C" fn suspend_thread_list(
    env: *mut JvmtiNativeEnv,
    request_count: JInt,
    request_list: *const JObject,
    results: *mut JInt,
) -> JInt {
    let (shared, handles) = match thread_list(env, request_count, request_list, results) {
        Ok(list) => list,
        Err(code) => return code,
    };
    let mut marked = Vec::new();
    let mut current_marked = None;
    for (at, &handle) in handles.iter().enumerate() {
        let code = match suspension_target(&shared, handle) {
            Ok((tid, is_current)) => {
                let code = mark_suspended(&shared, tid);
                if code == ERR_NONE {
                    if is_current {
                        current_marked = Some(tid);
                    } else {
                        marked.push(tid);
                    }
                }
                code
            }
            Err(code) => code,
        };
        // SAFETY: the agent's results array has `request_count` entries.
        unsafe { *results.add(at) = code };
    }
    for tid in marked {
        await_suspended(&shared, tid, false);
    }
    if let Some(tid) = current_marked {
        await_suspended(&shared, tid, true);
    }
    ERR_NONE
}

/// Slot 93: `ResumeThreadList(env, request_count, request_list, results)`
/// (wave 45; `can_suspend`): each thread's own answer in `results`.
pub(in crate::jvmti::native_env) extern "C" fn resume_thread_list(
    env: *mut JvmtiNativeEnv,
    request_count: JInt,
    request_list: *const JObject,
    results: *mut JInt,
) -> JInt {
    let (shared, handles) = match thread_list(env, request_count, request_list, results) {
        Ok(list) => list,
        Err(code) => return code,
    };
    for (at, &handle) in handles.iter().enumerate() {
        let code = match suspension_target(&shared, handle) {
            Ok((tid, _)) => mark_resumed(&shared, tid),
            Err(code) => code,
        };
        // SAFETY: the agent's results array has `request_count` entries.
        unsafe { *results.add(at) = code };
    }
    ERR_NONE
}

/// Slot 20: `NotifyFramePop(env, thread, depth)` (wave 45;
/// `can_generate_frame_pop_events`): a `FramePop` event when the frame at
/// `depth` of the listing ([`listed_rows`]) is popped, by a return or an
/// exception, posted by the interpreter as it pops it
/// (`jvmti_events::fire_jvmti_frame_pop_if_requested`, from
/// `JvmThread::frame_pop_requests`). Codes as HotSpot 25.0.3's (measured,
/// `tools/probes/interp/L1/L1W45JvmtiNotifyFramePop.java`): another thread
/// not suspended is `THREAD_NOT_SUSPENDED`, a negative depth
/// `ILLEGAL_ARGUMENT`, a depth past the bottom `NO_MORE_FRAMES`, a native
/// method's frame `OPAQUE_FRAME`, a second request for one frame `DUPLICATE`.
/// A compiled activation, and an interpreter frame whose body runs compiled,
/// are `OPAQUE_FRAME` here (HotSpot deoptimizes them). A suspended thread
/// takes the request itself while parked at a suspend point
/// (`debug::run_on_parked_thread`); one suspended while blocked in a native
/// region is `JVMTI_ERROR_NOT_AVAILABLE`, or, with
/// `DebuggerGates::c_frame_control` (wave 61), takes it at its wake
/// ([`frame_pop_through_window`]).
pub(in crate::jvmti::native_env) extern "C" fn notify_frame_pop(env: *mut JvmtiNativeEnv, thread: JObject, depth: JInt) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_GENERATE_FRAME_POP_EVENTS) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let (tid, is_current) = match suspension_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if !is_current && !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    if depth < 0 {
        return ERR_ILLEGAL_ARGUMENT;
    }
    if is_current {
        let Some(current) = jni::current_jvm_thread_of(&shared) else {
            return ERR_UNATTACHED_THREAD;
        };
        // SAFETY: the calling thread's own, live `JvmThread`, which is inside
        // this call and does not change its frames meanwhile.
        return request_frame_pop(&shared, unsafe { &mut *current }, depth);
    }
    // Interpreter round i2 wave 61 (lane L1): a thread suspended while blocked
    // in a native takes the request at its wake ([`frame_pop_through_window`]).
    if shared.debug.debugger_gates.c_frame_control() {
        let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
            frame_pop_through_window(&shared, held, depth)
        };
        let asked = on_stopped_thread(
            &shared,
            tid,
            move |shared: &SharedVm, thread: &mut JvmThread| {
                request_frame_pop(shared, thread, depth)
            },
            window,
        );
        return match asked {
            Ok((code, _)) => code,
            Err(code) => code,
        };
    }
    let asked = gc_safe(&shared, || {
        crate::debug::run_on_parked_thread(&shared, Some(tid), move |shared, thread| {
            request_frame_pop(shared, thread, depth)
        })
    });
    asked.unwrap_or(ERR_NOT_AVAILABLE)
}

/// [`notify_frame_pop`] on `thread`, the thread that runs this.
pub(in crate::jvmti::native_env) fn request_frame_pop(shared: &SharedVm, thread: &mut JvmThread, depth: JInt) -> JInt {
    let Ok(depth) = usize::try_from(depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let rows = listed_rows(shared, thread, true);
    let Some(row) = rows.get(depth) else {
        return ERR_NO_MORE_FRAMES;
    };
    let Some(index) = row.frame.and_then(|frame| u32::try_from(frame).ok()) else {
        return ERR_OPAQUE_FRAME;
    };
    if thread.frame_pop_requests.contains(&index) {
        return ERR_DUPLICATE;
    }
    thread.frame_pop_requests.push(index);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_FRAME_POP] requested tid={} depth={depth} frame={index}",
            thread.thread_id.0
        );
    }
    ERR_NONE
}

// ---------------------------------------------------------------------------
// Frame control (interpreter round i2 wave 61, lane L1)
// ---------------------------------------------------------------------------
//
// `PopFrame` (80), `ForceEarlyReturn*` (81-86), `ClearAllFramePops` (67) and
// `NotifyFramePop` of a thread suspended while blocked in a native, behind
// `DebuggerGates::c_frame_control` (`CRATONVM_JVMTI_C_FRAME_CONTROL`; off,
// every slot answers `JVMTI_ERROR_NOT_AVAILABLE` as before and the two
// capabilities are not potential). Proposal:
// `docs/feature-designs/jvmti-delivery-threading.md`.
// MEASURED on HotSpot 25.0.3 (`tools/probes/interp/L1/L1W61JvmtiFrameControl.java`).
//
// A suspended thread parked at an interpreter suspend point takes the
// request itself (`debug::run_on_parked_thread`): the forced return or the
// pop is recorded there and taken when the thread resumes, as HotSpot takes
// both (the JDWP machinery of waves 43-44, `debug::early_return`,
// `debug::pop_frames`). The calling thread's own top frame is the native
// that calls the function, `OPAQUE_FRAME` on HotSpot too. A request from a
// `Breakpoint` or `SingleStep` callback on the calling thread (whose top
// frame is the event's Java frame on HotSpot) is served since wave 62 with
// `DebuggerGates::c_callback_frame_control` ([`FrameTarget::Current`]); from
// a `MethodEntry` callback since the wave-129 flip of
// `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL` (default ON; `=0` answers
// `OPAQUE_FRAME` there again; stage 2 of
// `docs/feature-designs/real-jdk-mode.md`).
// A thread suspended while blocked on a monitor entry is served since
// interpreter round i6 wave 134, lane L1, through its inspection window
// (`CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`, default ON, `=0` answers
// `OPAQUE_FRAME` again; [`frame_op_on_parked`], [`blocked_entry_frame`];
// `docs/feature-designs/jvmti-delivery-threading.md`).

/// The VM of a frame-control call: `JVMTI_ERROR_NOT_AVAILABLE` while its
/// `DebuggerGates::c_frame_control` is off, then the live phase and the
/// capability `bit` of word `word`.
pub(in crate::jvmti::native_env) fn frame_control_vm(
    env: *mut JvmtiNativeEnv,
    word: usize,
    bit: u32,
) -> Result<Arc<SharedVm>, JInt> {
    let shared = live_phase_vm(env)?;
    if !shared.debug.debugger_gates.c_frame_control() {
        return Err(ERR_NOT_AVAILABLE);
    }
    live_phase_vm_with(env, word, bit)
}

/// Run `work` on suspended thread `tid` while it is parked
/// (`debug::run_on_parked_thread`); its refusal is a JDWP error number, the
/// JVMTI one for these codes. A thread suspended but not parked (blocked in
/// a native, or not yet at a suspend point) is `OPAQUE_FRAME`: HotSpot
/// answers it for a native top frame and for a thread in `Object.wait`.
///
/// Interpreter round i6 wave 134, lane L1
/// (`CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL`, default ON, `=0` off;
/// `docs/feature-designs/jvmti-delivery-threading.md`):
/// a suspended thread blocked entering a monitor, whose top frame is the
/// Java frame at the entry on HotSpot, gets `window` through its inspection
/// window instead ([`blocked_entry_frame`]); a window that is not open (or
/// another reader's) keeps the `OPAQUE_FRAME` answer.
pub(in crate::jvmti::native_env) fn frame_op_on_parked<F, W>(shared: &SharedVm, tid: u64, work: F, window: W) -> JInt
where
    F: FnOnce(&SharedVm, &mut JvmThread) -> Result<(), u16> + Send + 'static,
    W: FnOnce(&crate::runtime::interpreter::BlockedFrames<'_>) -> JInt,
{
    let ran = gc_safe(shared, || {
        crate::debug::run_on_parked_thread(shared, Some(tid), work)
    });
    match ran {
        Ok(Ok(())) => ERR_NONE,
        Ok(Err(code)) => JInt::from(code),
        Err(crate::debug::ParkedError::NotParked) => {
            if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
                return ERR_THREAD_NOT_SUSPENDED;
            }
            if blocked_entry_frame_control_on(shared) {
                if let Some(code) =
                    crate::runtime::interpreter::with_blocked_frames(shared, tid, window)
                {
                    return code;
                }
            }
            ERR_OPAQUE_FRAME
        }
        Err(crate::debug::ParkedError::Busy) => ERR_THREAD_NOT_SUSPENDED,
        Err(crate::debug::ParkedError::Lost) => ERR_INTERNAL,
    }
}

/// The thread a frame-control function acts on ([`frame_control_target`]).
#[derive(Clone, Copy)]
pub(in crate::jvmti::native_env) enum FrameTarget {
    /// Another thread, suspended: the request runs on it while it is parked
    /// ([`frame_op_on_parked`]).
    Suspended(u64),
    /// The calling thread, inside a `Breakpoint` or `SingleStep` callback
    /// whose frame it may act on (interpreter round i2 wave 62, lane L1;
    /// `DebuggerGates::c_callback_frame_control`): the request is checked
    /// against the callback point the interpreter entered
    /// ([`frame_op_on_current`]).
    Current(u64),
}

/// The thread a frame-control function names, after HotSpot's checks
/// (`check_non_suspended_or_opaque_frame`): another thread not suspended,
/// `THREAD_NOT_SUSPENDED`; the calling thread, whose top frame is the native
/// calling this, `OPAQUE_FRAME` -- unless (wave 62, with
/// `DebuggerGates::c_callback_frame_control`) it calls from an event
/// callback, where its top frame is the event's Java frame, as on HotSpot;
/// that is decided against the callback point ([`frame_op_on_current`]).
pub(in crate::jvmti::native_env) fn frame_control_target(shared: &SharedVm, thread: JObject) -> Result<FrameTarget, JInt> {
    let (tid, is_current) = suspension_target(shared, thread)?;
    if is_current {
        if shared.debug.debugger_gates.c_callback_frame_control() {
            return Ok(FrameTarget::Current(tid));
        }
        return Err(ERR_OPAQUE_FRAME);
    }
    if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return Err(ERR_THREAD_NOT_SUSPENDED);
    }
    Ok(FrameTarget::Suspended(tid))
}

/// Run `work` on the calling thread's own `JvmThread`, inside an event
/// callback (wave 62, [`FrameTarget::Current`]); its refusal is a JDWP error
/// number, the JVMTI one for these codes. Outside a callback point the
/// checks in `work` answer `OPAQUE_FRAME`, wave 61's answer for the calling
/// thread.
pub(in crate::jvmti::native_env) fn frame_op_on_current<F>(shared: &SharedVm, work: F) -> JInt
where
    F: FnOnce(&SharedVm, &JvmThread) -> Result<(), u16>,
{
    let Some(current) = jni::current_jvm_thread_of(shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    // SAFETY: the calling thread's own, live `JvmThread`, read inside this
    // call (as `clear_all_frame_pops` reaches it); the interpreter that runs
    // the callback does not touch it until the callback returns.
    match work(shared, unsafe { &*current }) {
        Ok(()) => ERR_NONE,
        Err(code) => JInt::from(code),
    }
}

/// Slot 80: `PopFrame(env, thread)` (wave 61; `can_pop_frame`): the top
/// frame of a suspended thread is popped when it resumes and the invoke that
/// called it runs again (`debug::pop_frames::request_jvmti_pop`). HotSpot's
/// answers (MEASURED): a NULL `thread` is `INVALID_THREAD` (not the calling
/// thread, unlike the other thread functions), a second `PopFrame` before
/// the resume `INTERNAL`, a thread whose only frame is the top one
/// `NO_MORE_FRAMES`, a native top frame `OPAQUE_FRAME`; the thread lists the
/// popped frame until it resumes; a `NotifyFramePop` of the popped frame
/// posts nothing.
pub(in crate::jvmti::native_env) extern "C" fn pop_frame(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match frame_control_vm(env, 0, CAN_POP_FRAME) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if thread == 0 {
        return ERR_INVALID_THREAD;
    }
    match frame_control_target(&shared, thread) {
        Ok(FrameTarget::Suspended(tid)) => frame_op_on_parked(
            &shared,
            tid,
            move |shared, thread| crate::debug::pop_frames::request_jvmti_pop(shared, thread, tid),
            // Wave 134 (lane L1): blocked entering a monitor.
            |held| pop_through_window(&shared, held),
        ),
        // Wave 62: from the calling thread's own event callback.
        Ok(FrameTarget::Current(tid)) => frame_op_on_current(&shared, |shared, thread| {
            crate::debug::pop_frames::request_jvmti_pop_in_callback(shared, thread, tid)
        }),
        Err(code) => code,
    }
}

/// A `ForceEarlyReturn*` value as the agent passed it.
#[derive(Clone, Copy)]
pub(in crate::jvmti::native_env) enum EarlyValue {
    Void,
    Plain(crate::types::Value),
    Object(JObject),
}

/// Slots 81-86, `ForceEarlyReturn{Object,Int,Long,Float,Double,Void}` (wave
/// 61; `can_force_early_return`): the top method of a suspended thread
/// returns `value` when the thread resumes, without running on
/// (`debug::early_return::record_jvmti`, taken where a JDWP
/// `ThreadReference.ForceEarlyReturn` is). `kind` is the family (`I`, `J`,
/// `F`, `D`, `L`, `V`). HotSpot's answers (MEASURED): a value of another kind
/// than the return type, or an object of another class, `TYPE_MISMATCH`
/// (`Int` serves `boolean`, `byte`, `char`, `short` and `int`); a second
/// request before the resume `INTERNAL`; a native top frame `OPAQUE_FRAME`.
pub(in crate::jvmti::native_env) fn force_early_return(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    kind: u8,
    value: EarlyValue,
) -> JInt {
    use crate::debug::early_return::JvmtiForced;
    let shared = match frame_control_vm(env, 1, CAN_FORCE_EARLY_RETURN) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let target = match frame_control_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    let forced = match value {
        EarlyValue::Void => JvmtiForced::Void,
        EarlyValue::Plain(v) => JvmtiForced::Plain(v),
        EarlyValue::Object(0) => JvmtiForced::Plain(crate::types::Value::Object(None)),
        EarlyValue::Object(handle) => match local_to_global(&shared, handle) {
            Ok(global) => JvmtiForced::Global(global),
            Err(code) => return code,
        },
    };
    let code = match target {
        FrameTarget::Suspended(tid) => frame_op_on_parked(
            &shared,
            tid,
            move |shared, thread| {
                crate::debug::early_return::record_jvmti(shared, thread, tid, kind, forced)
            },
            // Wave 134 (lane L1): blocked entering a monitor.
            |held| forced_return_through_window(&shared, held, kind, forced),
        ),
        // Wave 62: from the calling thread's own event callback, recorded
        // at its callback point and taken as the callback returns.
        FrameTarget::Current(tid) => frame_op_on_current(&shared, |shared, thread| {
            crate::debug::early_return::record_jvmti(shared, thread, tid, kind, forced)
        }),
    };
    if code != ERR_NONE {
        if let JvmtiForced::Global(global) = forced {
            release_global(&shared, global);
        }
    }
    code
}

pub(in crate::jvmti::native_env) extern "C" fn force_early_return_object(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: JObject,
) -> JInt {
    force_early_return(env, thread, b'L', EarlyValue::Object(value))
}

pub(in crate::jvmti::native_env) extern "C" fn force_early_return_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: JInt,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'I',
        EarlyValue::Plain(crate::types::Value::Int(value)),
    )
}

pub(in crate::jvmti::native_env) extern "C" fn force_early_return_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: i64,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'J',
        EarlyValue::Plain(crate::types::Value::Long(value)),
    )
}

pub(in crate::jvmti::native_env) extern "C" fn force_early_return_float(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: f32,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'F',
        EarlyValue::Plain(crate::types::Value::Float(value)),
    )
}

pub(in crate::jvmti::native_env) extern "C" fn force_early_return_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    value: f64,
) -> JInt {
    force_early_return(
        env,
        thread,
        b'D',
        EarlyValue::Plain(crate::types::Value::Double(value)),
    )
}

pub(in crate::jvmti::native_env) extern "C" fn force_early_return_void(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    force_early_return(env, thread, b'V', EarlyValue::Void)
}

/// Slot 67: `ClearAllFramePops(env, thread)` (JDK 25's slot 67; wave 61,
/// `can_generate_frame_pop_events`): drop every `NotifyFramePop` request of
/// the calling thread or of a suspended one, which posts none of their
/// `FramePop`s. MEASURED: another thread not suspended is
/// `THREAD_NOT_SUSPENDED`; the calling thread, and a second call, `NONE`. A
/// thread suspended while blocked in a native drops them at its wake.
pub(in crate::jvmti::native_env) extern "C" fn clear_all_frame_pops(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match frame_control_vm(env, 0, CAN_GENERATE_FRAME_POP_EVENTS) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let (tid, is_current) = match suspension_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if is_current {
        let Some(current) = jni::current_jvm_thread_of(&shared) else {
            return ERR_UNATTACHED_THREAD;
        };
        // SAFETY: the calling thread's own, live `JvmThread`, inside this call.
        unsafe { &mut *current }.frame_pop_requests.clear();
        return ERR_NONE;
    }
    if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
        crate::debug::inspect::defer_jvmti_clear_frame_pops(&shared, held.tid());
        ERR_NONE
    };
    let cleared = on_stopped_thread(
        &shared,
        tid,
        |_: &SharedVm, thread: &mut JvmThread| {
            thread.frame_pop_requests.clear();
            ERR_NONE
        },
        window,
    );
    match cleared {
        Ok((code, _)) => code,
        Err(code) => code,
    }
}

/// [`notify_frame_pop`] of a thread suspended while blocked in a native
/// (wave 61), through its inspection window: the frame at `depth` of the
/// listing the window publishes, recorded for the thread's wake
/// (`debug::inspect::defer_jvmti_frame_pop`). The listing's native top row
/// and a compiled frame are `OPAQUE_FRAME`, as for the parked thread; so is
/// an unmounted virtual thread's frame (its continuation is not the thread's
/// stack).
pub(in crate::jvmti::native_env) fn frame_pop_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    depth: JInt,
) -> JInt {
    let Ok(depth) = usize::try_from(depth) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    let rows = other_rows(shared, held.rows.clone());
    let Some(row) = rows.get(depth) else {
        return ERR_NO_MORE_FRAMES;
    };
    if held.unmounted() {
        return ERR_OPAQUE_FRAME;
    }
    let index = match local_frame_index(row) {
        Ok(index) => index,
        Err(code) => return code,
    };
    let Some(frame) = held.frames().get(index) else {
        return ERR_OPAQUE_FRAME;
    };
    if !row_names_frame(&row.raw, frame) {
        return ERR_NOT_AVAILABLE;
    }
    let recorded = crate::debug::inspect::defer_jvmti_frame_pop(
        shared,
        held.tid(),
        index,
        held.frames().len(),
        row.raw.class_id,
        row.raw.method_id,
    );
    if recorded {
        ERR_NONE
    } else {
        ERR_DUPLICATE
    }
}

/// Is `CRATONVM_JVMTI_BLOCKED_ENTRY_FRAME_CONTROL` in force for this VM
/// (interpreter round i6 wave 134, lane L1)? It needs the dispatch loop's
/// reader of the agent-request word, `CRATONVM_JVMTI_METHOD_ENTRY_FRAME_CONTROL`.
pub(in crate::jvmti::native_env) fn blocked_entry_frame_control_on(shared: &SharedVm) -> bool {
    shared.config.jvmti_blocked_entry_frame_control
}

/// The JVMTI error number `code` as the JDWP-numbered refusals of
/// `debug::early_return` / `debug::pop_frames` carry it (the same numbers
/// for these codes).
pub(in crate::jvmti::native_env) fn jdwp_numbered(code: JInt) -> u16 {
    u16::try_from(code).unwrap_or(113) // 113: JVMTI_ERROR_INTERNAL
}

/// The two ways a thread blocked entering a monitor stands at its top frame
/// (interpreter round i6 wave 134, lane L1): at a `monitorenter` (its
/// `last_instr_pc`), or at bytecode 0 of a `synchronized` method whose
/// frame was pushed before its contended monitor (`frame_first_entry`: the
/// method is synchronized and the frame owns no monitor yet,
/// `vm_exec::SyncEntry::Deferred`).
pub(in crate::jvmti::native_env) fn blocked_at_monitor_entry(code_at_last: Option<u8>, frame_first_entry: bool) -> bool {
    const MONITORENTER: u8 = 0xc2;
    code_at_last == Some(MONITORENTER) || frame_first_entry
}

/// Can a `PopFrame` / `ForceEarlyReturn*` of the top frame of the thread
/// whose frames `held` holds be recorded for its wake (interpreter round i6
/// wave 134, lane L1)? The thread is BLOCKED entering a monitor (block state
/// 2), mounted, and its listing's top row is that interpreter frame, run
/// interpreted, standing at the monitor entry ([`blocked_at_monitor_entry`]):
/// HotSpot's top frame then is that Java frame. Anything else --
/// `Object.wait`, a sleep or a native whose row heads the listing, a
/// compiled activation, a block of another kind -- is `OPAQUE_FRAME`, the
/// answer before this wave. `with_caller`: the `PopFrame` checks of the
/// caller's row too (none `NO_MORE_FRAMES`; a native, compiled or spliced row
/// `OPAQUE_FRAME`, as HotSpot's two-Java-frames test answers it).
pub(in crate::jvmti::native_env) fn blocked_entry_frame(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    with_caller: bool,
) -> Result<(), JInt> {
    if held.unmounted()
        || shared
            .threads
            .thread_registry
            .java_block_state(ThreadId(held.tid()))
            != 2
    {
        return Err(ERR_OPAQUE_FRAME);
    }
    let rows = other_rows(shared, held.rows.clone());
    let top_row = rows.first().ok_or(ERR_OPAQUE_FRAME)?;
    let index = local_frame_index(top_row)?;
    let frames = held.frames();
    if index + 1 != frames.len() {
        return Err(ERR_OPAQUE_FRAME);
    }
    let frame = &frames[index];
    if !row_names_frame(&top_row.raw, frame) || frame.runs_obsolete_method() {
        return Err(ERR_OPAQUE_FRAME);
    }
    let frame_first_entry = frame.monitor_on_exit.is_none()
        && frame.pc == 0
        && with_method(shared, top_row.info.method, |_, m| m.is_synchronized()).unwrap_or(false);
    if !blocked_at_monitor_entry(frame.code.get(frame.last_instr_pc).copied(), frame_first_entry) {
        return Err(ERR_OPAQUE_FRAME);
    }
    if with_caller {
        let caller_row = rows.get(1).ok_or(ERR_NO_MORE_FRAMES)?;
        let caller = local_frame_index(caller_row)?;
        if caller + 1 != index {
            return Err(ERR_OPAQUE_FRAME);
        }
    }
    Ok(())
}

/// [`pop_frame`] of a suspended thread blocked entering a monitor, through
/// its inspection window (interpreter round i6 wave 134, lane L1): checked
/// here ([`blocked_entry_frame`], `debug::pop_frames::checked_blocked_pop`)
/// and recorded for its wake (`debug::early_return::defer_blocked_pop`),
/// where it becomes the record the thread's next bytecode takes. A second
/// `PopFrame` before the resume is `INTERNAL`, as for a parked thread.
pub(in crate::jvmti::native_env) fn pop_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
) -> JInt {
    let frames = held.frames();
    let checked = match blocked_entry_frame(shared, held, true) {
        Ok(()) => crate::debug::pop_frames::checked_blocked_pop(
            shared,
            frames,
            held.no_continuation_adapters(),
        ),
        Err(code) => Err(jdwp_numbered(code)),
    };
    match crate::debug::early_return::defer_blocked_pop(shared, held.tid(), frames, checked) {
        Ok(()) => ERR_NONE,
        Err(code) => JInt::from(code),
    }
}

/// [`force_early_return`] of a suspended thread blocked entering a monitor,
/// through its inspection window (interpreter round i6 wave 134, lane L1):
/// the top frame judged here ([`blocked_entry_frame`]), the value and the
/// pending check in `debug::early_return::defer_blocked_forced_return`,
/// recorded for the thread's wake. An object value is resolved inside the
/// foreign-thread entry, so no collection moves it while its class is read.
pub(in crate::jvmti::native_env) fn forced_return_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    kind: u8,
    forced: crate::debug::early_return::JvmtiForced,
) -> JInt {
    let top_servable = blocked_entry_frame(shared, held, false).is_ok();
    let _fx = jni::ForeignJniEntry::enter();
    match crate::debug::early_return::defer_blocked_forced_return(
        shared,
        held.tid(),
        held.frames(),
        top_servable,
        kind,
        forced,
    ) {
        Ok(()) => ERR_NONE,
        Err(code) => JInt::from(code),
    }
}

/// Slot 17: `GetThreadState(env, thread, thread_state_ptr)` (wave 45; no
/// capability). A NULL `thread` is the calling thread. The answer carries
/// `java.lang.Thread.State`'s bits (from the thread's blocking kind,
/// `ThreadRegistry::java_block_state`: `RUNNABLE`, `BLOCKED_ON_MONITOR_ENTER`,
/// `WAITING` with `INDEFINITELY` or `WITH_TIMEOUT`), `SUSPENDED` (a JDWP or a
/// JVMTI suspension, `DebugState::is_thread_suspended`) and `INTERRUPTED`;
/// a waiting thread's kind, `SLEEPING`, `IN_OBJECT_WAIT` or `PARKED`
/// (wave 57, [`wait_kind`]); not yet `IN_NATIVE`.
/// A terminated thread is `TERMINATED`, one never started 0.
pub(in crate::jvmti::native_env) extern "C" fn get_thread_state(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    state_ptr: *mut JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if state_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let lookup = if thread == 0 {
        match suspension_target(&shared, 0) {
            Ok((tid, _)) => ThreadLookup::Live(tid),
            Err(code) => return code,
        }
    } else {
        match lookup_thread(&shared, thread) {
            Ok(lookup) => lookup,
            Err(code) => return code,
        }
    };
    let state = match lookup {
        ThreadLookup::Unstarted => 0,
        ThreadLookup::Dead => THREAD_STATE_TERMINATED,
        ThreadLookup::Live(tid) => live_thread_state(&shared, tid),
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *state_ptr = state };
    ERR_NONE
}

/// [`get_thread_state`]'s answer for live thread `tid`.
pub(in crate::jvmti::native_env) fn live_thread_state(shared: &SharedVm, tid: u64) -> JInt {
    let registry = &shared.threads.thread_registry;
    let id = ThreadId(tid);
    // Interpreter round i6 wave 132, lane L1 (i130-L1b's first next step,
    // `CRATONVM_JVMTI_THREAD_STATE_ONE_READ`, performance, default ON since i7, `=0` off): the
    // block state, the published wait kind and the interrupted flag from one
    // registry read, each the answer of its own read below.
    // Interpreter round i7 wave 135, lane L1 (`CRATONVM_JVMTI_STATE_SUSPEND_WORD`,
    // performance, default off): the thread's own suspended word, read in the
    // same registry read as the facts when there is one.
    let word_on = shared.debug.debugger_gates.jvmti_state_suspend_word();
    let mut suspend_word = None;
    let facts = shared
        .debug
        .debugger_gates
        .jvmti_thread_state_one_read()
        .then(|| {
            if word_on {
                let (facts, word) = registry.jvmti_state_facts_and_suspend_word(id);
                suspend_word = Some(word);
                facts
            } else {
                registry.jvmti_state_facts(id)
            }
        });
    let block = facts.map_or_else(|| registry.java_block_state(id), |(block, _, _)| block);
    let kind = || match facts {
        Some((_, Some(kind), _)) => published_wait_kind_bit(kind),
        _ => wait_kind(shared, id),
    };
    let mut state = THREAD_STATE_ALIVE
        | match block {
            1 => THREAD_STATE_WAITING | THREAD_STATE_WAITING_INDEFINITELY | kind(),
            2 => THREAD_STATE_BLOCKED_ON_MONITOR_ENTER,
            3 => THREAD_STATE_WAITING | THREAD_STATE_WAITING_WITH_TIMEOUT | kind(),
            _ => THREAD_STATE_RUNNABLE,
        };
    // Wave 58 (lane L1): a RUNNABLE thread in native code
    // ([`thread_in_native`]; only with `CRATONVM_JVMTI_C_IN_NATIVE`).
    if state & THREAD_STATE_RUNNABLE != 0
        && shared.debug.debugger_gates.c_in_native_armed()
        && thread_in_native(shared, tid)
    {
        state |= THREAD_STATE_IN_NATIVE;
    }
    let interrupted = facts.map_or_else(
        || {
            registry
                .get_interrupted_flag(id)
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        },
        |(_, _, interrupted)| interrupted,
    );
    if interrupted {
        state |= THREAD_STATE_INTERRUPTED;
    }
    // Interpreter round i3 wave 72, lane L6
    // (`CRATONVM_JVMTI_STATE_SUSPENSION_HINT`, default off): no thread is
    // suspended -- the hint every suspension mutator refreshes under the
    // debug-state lock -- so no `SUSPENDED` bit, without that lock (a sampler
    // asking many threads no longer serialises on the debugger's lock).
    let surely_not_suspended = shared.debug.debugger_gates.state_suspension_hint()
        && !shared
            .debug
            .suspension_hint
            .load(std::sync::atomic::Ordering::Acquire);
    // Wave 135 (lane L1): the per-thread word answers without the lock once
    // published; an unpublished one subscribes the thread under the lock.
    let suspended = if surely_not_suspended {
        false
    } else if word_on {
        let word = suspend_word.unwrap_or_else(|| registry.jvmti_state_suspend_word(id));
        state_suspend_word_answer(word)
            .unwrap_or_else(|| subscribe_state_suspend_word(shared, id))
    } else {
        shared.debug.debug_state.lock().is_thread_suspended(tid)
    };
    if suspended {
        state |= THREAD_STATE_SUSPENDED;
    }
    state
}

/// What a published C JVMTI state suspend word answers (interpreter round i7
/// wave 135, lane L1; `GcBlockState::jvmti_state_suspended`): `None` while it
/// is unpublished, and the reader asks the lock.
pub(in crate::jvmti::native_env) fn state_suspend_word_answer(word: u8) -> Option<bool> {
    use crate::threading::jvm_thread::{JVMTI_STATE_SUSPEND_NO, JVMTI_STATE_SUSPEND_YES};
    match word {
        JVMTI_STATE_SUSPEND_YES => Some(true),
        JVMTI_STATE_SUSPEND_NO => Some(false),
        _ => None,
    }
}

/// The first `GetThreadState` of thread `id` under
/// `CRATONVM_JVMTI_STATE_SUSPEND_WORD` (wave 135, lane L1): its block state is
/// fetched from the registry (no lock held), then, under the debug-state lock,
/// the thread is subscribed (its word written now and by every later
/// suspension mutator) and the lock's answer returned. A thread gone from the
/// registry is answered from the lock alone.
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn subscribe_state_suspend_word(shared: &SharedVm, id: ThreadId) -> bool {
    let block_state = shared.threads.thread_registry.gc_block_state_of(id);
    let mut ds = shared.debug.debug_state.lock();
    match block_state {
        Some(state) => ds.subscribe_state_suspend_word(id.0, state),
        None => ds.is_thread_suspended(id.0),
    }
}

/// The [`live_thread_state`] kind bit of a published wait kind
/// ([`wait_kind`]'s first answer; shared with the one-read facts of wave 132).
pub(in crate::jvmti::native_env) fn published_wait_kind_bit(kind: crate::threading::thread_registry::PublishedWaitKind) -> JInt {
    use crate::threading::thread_registry::PublishedWaitKind;
    match kind {
        PublishedWaitKind::Sleeping => THREAD_STATE_SLEEPING,
        PublishedWaitKind::ObjectWait => THREAD_STATE_IN_OBJECT_WAIT,
        PublishedWaitKind::Parked | PublishedWaitKind::Unrecorded => THREAD_STATE_PARKED,
    }
}

/// The kind bit of a WAITING / TIMED_WAITING thread's [`live_thread_state`]
/// (interpreter round i2 wave 57, lane L1; HotSpot 25.0.3 MEASURED,
/// `tools/probes/interp/L1/L1W57JvmtiWaitStates.java`): `IN_OBJECT_WAIT` in
/// `Object.wait` (also `Thread.join`, which waits on the thread), the
/// thread's published waiting monitor; `SLEEPING` in `Thread.sleep`, read
/// from the deposited top frame as JDWP's `ThreadReference.Status` reads it
/// (`debug::is_thread_sleep_frame`, `debug::frame_invokes_thread_sleep`: the
/// registered `Thread.sleep` native deposits no `Thread` frame); `PARKED`
/// otherwise (`LockSupport.park*`, and every other blocking native that
/// stands in for JDK code that parks).
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn wait_kind(shared: &SharedVm, id: ThreadId) -> JInt {
    let registry = &shared.threads.thread_registry;
    // Interpreter round i2 wave 64, lane L1 (`CRATONVM_PUBLISHED_ALL_WAIT_KINDS`):
    // every wait published its kind (a park, `Object.wait`, the served
    // `Thread.join`, a sleep), so one atomic load answers, with no per-thread
    // mutex; a timed served join reads `IN_OBJECT_WAIT`, as HotSpot's `join`
    // (a `wait` loop) does, where the waiting-monitor test read `PARKED`.
    if let Some(kind) = registry.published_wait_kind_of(id) {
        return published_wait_kind_bit(kind);
    }
    // Interpreter round i2 wave 65, lane L1 (`CRATONVM_SERVED_WAIT_KINDS`): a
    // served stand-in's monitor wait for JDK code that parks (the
    // `--compatible` `CountDownLatch.await`) publishes a waiting monitor too,
    // which would read `IN_OBJECT_WAIT`; HotSpot's AQS acquire is `PARKED`.
    if registry.published_monitor_parked(id) {
        return THREAD_STATE_PARKED;
    }
    if registry.peek_jmx_waiting_monitor(id).is_some() {
        return THREAD_STATE_IN_OBJECT_WAIT;
    }
    // Interpreter round i2 wave 63, lane L1 (`CRATONVM_PUBLISHED_WAIT_KIND`):
    // the sleep's own region says it is a sleep, whatever frame it was
    // entered from (a reflective call, a method handle, `TimeUnit.sleep`);
    // the frame test below read those PARKED.
    match registry.published_sleeping(id) {
        Some(true) => return THREAD_STATE_SLEEPING,
        Some(false) => return THREAD_STATE_PARKED,
        None => {}
    }
    let trace = registry.frame_trace_of(id);
    let Some(top) = trace.last() else {
        return THREAD_STATE_PARKED;
    };
    if crate::debug::is_thread_sleep_frame(top) {
        return THREAD_STATE_SLEEPING;
    }
    let cm = shared.classes.class_manager.read();
    if crate::debug::frame_invokes_thread_sleep(&cm, top) {
        THREAD_STATE_SLEEPING
    } else {
        THREAD_STATE_PARKED
    }
}

// ---------------------------------------------------------------------------
// Locals and monitors of the current thread (interpreter round i1 wave 44,
// lane L1)
// ---------------------------------------------------------------------------
//
// The local-variable functions (slots 21-30 and 155), `GetCurrentContendedMonitor`
// (11) and `GetOwnedMonitorStackDepthInfo` (153) answered
// `JVMTI_ERROR_NOT_AVAILABLE` whatever they were asked, where HotSpot answers
// `JVMTI_ERROR_MUST_POSSESS_CAPABILITY` without the capability and serves
// them with it. Each code below is HotSpot 25.0.3's, measured
// (`tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java`, run as an
// `-agentpath` agent from a JNI native): the phase, then the capability,
// then the thread, a negative depth (`ILLEGAL_ARGUMENT`), a NULL
// out-parameter (`NULL_POINTER`), another running thread
// (`THREAD_NOT_SUSPENDED`), a depth past the bottom (`NO_MORE_FRAMES`), a
// native method's frame or a compiled activation (`OPAQUE_FRAME`), then the
// slot ([`checked_local`]).

/// `jvmtiMonitorStackDepthInfo`: `{ jobject monitor; jint stack_depth; }`,
/// 16 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) struct MonitorDepthRow {
    pub(in crate::jvmti::native_env) monitor: JObject,
    pub(in crate::jvmti::native_env) stack_depth: JInt,
}

/// The kind a local-variable function reads or writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) enum LocalKind {
    Int,
    Long,
    Float,
    Double,
    Object,
}

impl LocalKind {
    /// Two slots: `long` and `double`.
    pub(in crate::jvmti::native_env) fn wide(self) -> bool {
        matches!(self, LocalKind::Long | LocalKind::Double)
    }

    /// The kind of a field descriptor's first byte (`Z`, `B`, `C`, `S` and
    /// `I` are all `Int`, as `GetLocalInt` takes all five).
    pub(in crate::jvmti::native_env) fn of_signature(first: u8) -> Self {
        match first {
            b'J' => LocalKind::Long,
            b'F' => LocalKind::Float,
            b'D' => LocalKind::Double,
            b'L' | b'[' => LocalKind::Object,
            _ => LocalKind::Int,
        }
    }
}

/// What a method's `LocalVariableTable` says of a slot at a bytecode index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) enum Declared {
    /// The method has no table.
    NoTable,
    /// The table has no entry for the slot that covers the index.
    NotLive,
    /// The covering entry's kind.
    Kind(LocalKind),
}

/// [`Declared`] for `slot` of `frame`'s method at the frame's current
/// bytecode (`last_instr_pc`: its invoke for a caller, the bytecode an event
/// is posted for on top). A frame running an obsolete method is read as one
/// without a table: its class's current table may describe other bytecode.
pub(in crate::jvmti::native_env) fn declared_local(shared: &SharedVm, frame: &crate::runtime::frame::Frame, slot: u16) -> Declared {
    if frame.runs_obsolete_method() {
        return Declared::NoTable;
    }
    let cm = shared.classes.class_manager.read();
    let Some(class) = cm.get_class(frame.class_id) else {
        return Declared::NoTable;
    };
    let Some(code) = class
        .find_method(frame.method_name(), frame.method_descriptor())
        .and_then(|m| m.code())
    else {
        return Declared::NoTable;
    };
    declared_in_code(class, code, frame.last_instr_pc, slot)}

/// [`Declared`] for `slot` at `bci` of the method whose `code` `class`
/// declares: [`declared_local`]'s table read, split out in j27 wave 5 (lane
/// JVMTIFRAME) for a compiled row, which has no `Frame`
/// ([`compiled_locals`]).
pub(in crate::jvmti::native_env) fn declared_in_code(
    class: &crate::classloading::Class,
    code: &cratonvm_reader::attribute::CodeAttribute,
    bci: usize,
    slot: u16,
) -> Declared {
    use cratonvm_reader::attribute::Attribute;
    let mut has_table = false;
    for attribute in &code.attributes {
        let Attribute::LocalVariableTable(entries) = attribute else {
            continue;
        };
        has_table = true;
        // Interpreter round i5 wave 100, lane L1 (page
        // `interpreter-i100-L1-jvmti-local-scope-end-is-inclusive-FIXED-20261016.md`): the END is
        // inclusive and the first entry in table order wins, as HotSpot's
        // `VM_BaseGetOrSetLocal::check_slot_type_lvt` reads the table
        // (`start_bci <= bci && bci <= start_bci + length`). The JDWP side
        // already accepts the end (`commands::local_read_refusal`'s `pc == end`).
        for e in entries {
            let start = usize::from(e.start_pc);
            if e.index == slot && start <= bci && bci <= start + usize::from(e.length) {
                let first = class
                    .constant_pool
                    .get_utf8(e.descriptor_index)
                    .and_then(|d| d.bytes().next())
                    .unwrap_or(b'I');
                return Declared::Kind(LocalKind::of_signature(first));
            }
        }
    }
    if has_table {
        Declared::NotLive
    } else {
        Declared::NoTable
    }
}

/// The `LocalVariableTable` signature of the reference variable covering
/// `slot` of `frame`'s method at the frame's current bytecode, as
/// [`declared_local`] finds it; `None` without a table, for an obsolete
/// method, a slot no entry covers, or a primitive variable.
pub(in crate::jvmti::native_env) fn declared_reference_signature(
    shared: &SharedVm,
    frame: &crate::runtime::frame::Frame,
    slot: JInt,
) -> Option<String> {
    use cratonvm_reader::attribute::Attribute;
    let slot = u16::try_from(slot).ok()?;
    if frame.runs_obsolete_method() {
        return None;
    }
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(frame.class_id)?;
    let code = class
        .find_method(frame.method_name(), frame.method_descriptor())
        .and_then(|m| m.code())?;
    let bci = frame.last_instr_pc;
    for attribute in &code.attributes {
        let Attribute::LocalVariableTable(entries) = attribute else {
            continue;
        };
        for e in entries {
            let start = usize::from(e.start_pc);
            if e.index == slot && start <= bci && bci <= start + usize::from(e.length) {
                let signature = class.constant_pool.get_utf8(e.descriptor_index)?;
                return matches!(signature.bytes().next(), Some(b'L' | b'['))
                    .then(|| signature.to_string());
            }
        }
    }
    None
}

/// Does `obj`, a non-null value of `SetLocalObject`, fit the variable whose
/// signature is `signature`? HotSpot's `check_slot_type_lvt` refuses one
/// that does not with `TYPE_MISMATCH` (`is_assignable`, by name;
/// `debug::inspect::object_fits_local_signature`). Interpreter round i4 wave
/// 84, lane L1: unchecked before, so an agent could store a `String` into an
/// `Integer` local and the method's next `getfield` or `invokevirtual` ran
/// on an object of the wrong class. The callers hold no class-manager guard
/// but may be inside a foreign JNI entry, so the class manager is only tried
/// (it cannot block); a write that finds it write-locked is let through, as
/// every write was before.
pub(in crate::jvmti::native_env) fn object_fits_declared(shared: &SharedVm, signature: Option<&str>, obj: ObjectRef) -> bool {
    let Some(signature) = signature else {
        return true;
    };
    let Some(cm) = shared.classes.class_manager.try_read_untracked() else {
        return true;
    };
    crate::debug::inspect::object_fits_local_signature(
        &cm,
        shared.mem.heap.class_id_of(obj),
        signature,
    )
}

/// May a `kind` local be read or written at `slot` (a `jint` from the agent)
/// of `frame`? The slot as a `u16`, or the error, in HotSpot 25.0.3's order
/// (measured, `L1W44JvmtiLocalsAndStack`):
///
/// 1. `INVALID_SLOT` for a slot outside the frame's locals (the second slot
///    of a `long` / `double` included);
/// 2. `TYPE_MISMATCH` when the slot holds a reference and the kind is not
///    `Object`, or the kind is `Object` and the slot holds none (an
///    `Object` read of a variable not yet assigned is `TYPE_MISMATCH`);
/// 3. with a `LocalVariableTable`: `INVALID_SLOT` when no entry for the slot
///    covers the frame's bytecode (javac's hidden copy of a `synchronized`
///    block's lock, the second slot of a `long`, a variable not yet
///    assigned read as an `int`), `TYPE_MISMATCH` when the covering entry
///    declares another kind.
pub(in crate::jvmti::native_env) fn checked_local(
    declared: Declared,
    frame: &crate::runtime::frame::Frame,
    slot: JInt,
    kind: LocalKind,
) -> Result<u16, JInt> {
    let slot = u16::try_from(slot).map_err(|_| ERR_INVALID_SLOT)?;
    let last = usize::from(slot) + usize::from(kind.wide());
    if last >= frame.locals_len() {
        return Err(ERR_INVALID_SLOT);
    }
    let tag = frame.get_local_tag(usize::from(slot));
    let holds_reference = tag == crate::types::VTAG_OBJECT || tag == crate::types::VTAG_NULL;
    if holds_reference != (kind == LocalKind::Object) {
        return Err(ERR_TYPE_MISMATCH);
    }
    match declared {
        Declared::NoTable => Ok(slot),
        Declared::NotLive => Err(ERR_INVALID_SLOT),
        Declared::Kind(k) if k == kind => Ok(slot),
        Declared::Kind(_) => Err(ERR_TYPE_MISMATCH),
    }
}

/// The VM, when `env` is in the live phase and possesses the capability
/// `bit` of word `word`.
pub(in crate::jvmti::native_env) fn live_phase_vm_with(
    env: *mut JvmtiNativeEnv,
    word: usize,
    bit: u32,
) -> Result<Arc<SharedVm>, JInt> {
    let shared = live_phase_vm(env)?;
    let possessed = live_env(env)?
        .state
        .lock()
        .capabilities
        .get(word)
        .is_some_and(|w| w & bit != 0);
    if !possessed {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    Ok(shared)
}

/// The frame a local-variable function names, after the checks every one of
/// them makes (see the section note).
pub(in crate::jvmti::native_env) enum LocalTarget {
    /// The calling thread's own `JvmThread` and its listing row at the depth.
    Current(*mut JvmThread, ListedRow),
    /// Another thread, suspended, and the depth in its listing (interpreter
    /// round i1 wave 46, lane L1; [`other_local`]).
    Other(u64, usize),
}

/// The frame a local-variable function names: `(vm, the frame)`, after the
/// checks every one of them makes (see the section note). `out_is_null`: the
/// function's out-parameter (or, for a write, nothing: `false`) is NULL.
pub(in crate::jvmti::native_env) fn local_row(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    out_is_null: bool,
) -> Result<(Arc<SharedVm>, LocalTarget), JInt> {
    let shared = live_phase_vm_with(env, 0, CAN_ACCESS_LOCAL_VARIABLES)?;
    let current = jni::current_jvm_thread_of(&shared).ok_or(ERR_UNATTACHED_THREAD)?;
    // SAFETY: the calling thread's own, live `JvmThread`; `thread_id` is an
    // immutable `Copy` field.
    let current_tid = unsafe { (*current).thread_id.0 };
    let other = if thread == 0 {
        None
    } else {
        Some(thread_id_of(&shared, thread)?).filter(|&tid| tid != current_tid)    };
    if depth < 0 {
        return Err(ERR_ILLEGAL_ARGUMENT);
    }
    if out_is_null {
        return Err(ERR_NULL_POINTER);
    }
    let depth = usize::try_from(depth).map_err(|_| ERR_ILLEGAL_ARGUMENT)?;
    // Another thread's frames may be read only while it is suspended:
    // HotSpot's answer for a running thread. Wave 46 (lane L1): a suspended
    // one's are read and written where they hold still ([`other_local`]),
    // as HotSpot serves them (`L1W46JvmtiOtherThreadLocalsAndMonitors`).
    if let Some(tid) = other {
        if !shared.debug.debug_state.lock().is_thread_suspended(tid) {
            return Err(ERR_THREAD_NOT_SUSPENDED);
        }
        return Ok((shared, LocalTarget::Other(tid, depth)));
    }
    // SAFETY: as above; the thread is this one, inside this call.
    let rows = listed_rows(&shared, unsafe { &*current }, true);
    let row = rows.into_iter().nth(depth).ok_or(ERR_NO_MORE_FRAMES)?;
    Ok((shared, LocalTarget::Current(current, row)))
}

/// Read local `slot` of `kind` of the current thread's frame at `depth`:
/// the value, or the error. An object is answered as a local reference.
pub(in crate::jvmti::native_env) fn read_local(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    kind: LocalKind,
    out_is_null: bool,
) -> Result<LocalRead, JInt> {
    let (shared, current, row) = match local_row(env, thread, depth, out_is_null)? {
        (shared, LocalTarget::Current(current, row)) => (shared, current, row),
        (shared, LocalTarget::Other(tid, depth)) => {
            let read = other_local(&shared, tid, depth, LocalOp::Read(kind, slot))?;
            return read.map(|v| caller_value(&shared, v)).ok_or(ERR_INTERNAL);
        }
    };
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame, read inside
        // the foreign-thread entry (no collection moves its pins meanwhile).
        let _fx = jni::ForeignJniEntry::enter();
        // SAFETY: the calling thread's own `JvmThread`, inside this call.
        let value = proxy_local_read(unsafe { &*current }, record, slot, kind)?;
        return local_read_of(kind, value, |obj| {
            let prev = jni::replace_jni_context(&shared);
            let handle = obj.map_or(0, jni::new_local_handle);
            jni::restore_jni_context(prev);
            handle
        });
    }
    if let (None, Some(home)) = (row.frame, row.home) {
        // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd frame.
        let op = LocalOp::Read(kind, slot);
        return compiled_locals::current_home_read(&shared, current, &row.info, &home, op);
    }
    let index = row.frame.ok_or(ERR_OPAQUE_FRAME)?;
    // SAFETY: the calling thread's own `JvmThread`, inside this call: its
    // frames do not change while they are read.
    let frame = unsafe { (*current).frames.get(index) }.ok_or(ERR_OPAQUE_FRAME)?;
    let declared =
        u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(&shared, frame, s));
    // The frame's references are read, and a handle made, inside the
    // foreign-thread entry, so no collection moves them meanwhile.
    let _fx = jni::ForeignJniEntry::enter();
    let slot = checked_local(declared, frame, slot, kind)?;
    // gcf f2/f: a liveness-dead reference slot may hold a stale address.
    let live = frame.live_locals_mask_here();
    let value = crate::debug::debugger_local_for_read(&shared, frame, slot, &live);
    let read = match (kind, value) {
        (LocalKind::Int, crate::types::Value::Int(v)) => LocalRead::Int(v),
        (LocalKind::Long, crate::types::Value::Long(v)) => LocalRead::Long(v),
        (LocalKind::Float, crate::types::Value::Float(v)) => LocalRead::Float(v),
        (LocalKind::Double, crate::types::Value::Double(v)) => LocalRead::Double(v),
        (LocalKind::Object, crate::types::Value::Object(obj)) => {
            let prev = jni::replace_jni_context(&shared);
            let handle = obj.map_or(0, jni::new_local_handle);
            jni::restore_jni_context(prev);
            LocalRead::Object(handle)
        }
        _ => return Err(ERR_TYPE_MISMATCH),
    };
    Ok(read)
}

/// A value [`read_local`] read (or a local-variable function writes).
#[derive(Clone, Copy)]
pub(in crate::jvmti::native_env) enum LocalRead {
    Int(JInt),
    Long(i64),
    Float(f32),
    Double(f64),
    Object(JObject),
}

/// Write `value` (of `kind`) into local `slot` of the current thread's frame
/// at `depth`; an object `value` is a JNI handle (0: null).
pub(in crate::jvmti::native_env) fn write_local(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    kind: LocalKind,
    value: LocalRead,
) -> JInt {
    let (shared, current, row) = match local_row(env, thread, depth, false) {
        Ok((shared, LocalTarget::Current(current, row))) => (shared, current, row),
        Ok((shared, LocalTarget::Other(tid, depth))) => {
            return write_other_local(&shared, tid, depth, slot, kind, value);        }
        Err(code) => return code,
    };
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame.
        let _fx = jni::ForeignJniEntry::enter();
        let value = match value {
            LocalRead::Int(v) => Value::Int(v),
            LocalRead::Long(v) => Value::Long(v),
            LocalRead::Float(v) => Value::Float(v),
            LocalRead::Double(v) => Value::Double(v),
            LocalRead::Object(handle) => {
                let prev = jni::replace_jni_context(&shared);
                let obj = if handle == 0 {
                    None
                } else {
                    jni::jobject_to_obj(handle)
                };
                jni::restore_jni_context(prev);
                if handle != 0 && obj.is_none() {
                    return ERR_INVALID_OBJECT;
                }
                Value::Object(obj)
            }
        };
        // SAFETY: the calling thread's own `JvmThread`, inside this call.
        return match proxy_local_write(unsafe { &mut *current }, record, slot, kind, value) {
            Ok(()) => ERR_NONE,
            Err(code) => code,
        };
    }
    let Some(index) = row.frame else {
        return ERR_OPAQUE_FRAME;
    };
    let (declared, signature) = {
        // SAFETY: as in `read_local`.
        let Some(frame) = (unsafe { (*current).frames.get(index) }) else {
            return ERR_OPAQUE_FRAME;
        };
        (
            u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(&shared, frame, s)),
            (kind == LocalKind::Object)
                .then(|| declared_reference_signature(&shared, frame, slot))
                .flatten(),
        )
    };
    let _fx = jni::ForeignJniEntry::enter();
    // SAFETY: the calling thread's own `JvmThread`, inside this call: the
    // frame is below the native method running, and runs again only when
    // that returns.
    let Some(frame) = (unsafe { (*current).frames.get_mut(index) }) else {
        return ERR_OPAQUE_FRAME;
    };
    let slot = match checked_local(declared, frame, slot, kind) {
        Ok(slot) => slot,
        Err(code) => return code,
    };
    let value = match value {
        LocalRead::Int(v) => crate::types::Value::Int(v),
        LocalRead::Long(v) => crate::types::Value::Long(v),
        LocalRead::Float(v) => crate::types::Value::Float(v),
        LocalRead::Double(v) => crate::types::Value::Double(v),
        LocalRead::Object(handle) => {
            let prev = jni::replace_jni_context(&shared);
            let obj = if handle == 0 {
                None
            } else {
                jni::jobject_to_obj(handle)
            };
            jni::restore_jni_context(prev);
            if handle != 0 && obj.is_none() {
                return ERR_INVALID_OBJECT;
            }
            if obj.is_some_and(|o| !object_fits_declared(&shared, signature.as_deref(), o)) {
                return ERR_TYPE_MISMATCH;
            }
            crate::types::Value::Object(obj)
        }
    };
    frame.set_local(slot, value);
    ERR_NONE
}

/// Slot 21: `GetLocalObject(env, thread, depth, slot, value_ptr)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn get_local_object(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut JObject,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Object,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Object(handle)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = handle };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 22: `GetLocalInt(env, thread, depth, slot, value_ptr)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn get_local_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut JInt,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Int,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Int(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 23: `GetLocalLong(env, thread, depth, slot, value_ptr)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn get_local_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut i64,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Long,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Long(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 24: `GetLocalFloat(env, thread, depth, slot, value_ptr)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn get_local_float(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut f32,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Float,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Float(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 25: `GetLocalDouble(env, thread, depth, slot, value_ptr)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn get_local_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value_ptr: *mut f64,
) -> JInt {
    match read_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Double,
        value_ptr.is_null(),
    ) {
        Ok(LocalRead::Double(v)) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *value_ptr = v };
            ERR_NONE
        }
        Ok(_) => ERR_INTERNAL,
        Err(code) => code,
    }
}

/// Slot 26: `SetLocalObject(env, thread, depth, slot, value)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn set_local_object(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: JObject,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Object,
        LocalRead::Object(value),
    )
}

/// Slot 27: `SetLocalInt(env, thread, depth, slot, value)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn set_local_int(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: JInt,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Int,
        LocalRead::Int(value),
    )
}

/// Slot 28: `SetLocalLong(env, thread, depth, slot, value)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn set_local_long(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: i64,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Long,
        LocalRead::Long(value),
    )
}

/// Slot 29: `SetLocalFloat(env, thread, depth, slot, value)` (wave 44). The
/// `jfloat` comes in a floating-point register, as C passes it.
pub(in crate::jvmti::native_env) extern "C" fn set_local_float(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: f32,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Float,
        LocalRead::Float(value),
    )
}

/// Slot 30: `SetLocalDouble(env, thread, depth, slot, value)` (wave 44).
pub(in crate::jvmti::native_env) extern "C" fn set_local_double(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    slot: JInt,
    value: f64,
) -> JInt {
    write_local(
        env,
        thread,
        depth,
        slot,
        LocalKind::Double,
        LocalRead::Double(value),
    )
}

/// Slot 155: `GetLocalInstance(env, thread, depth, value_ptr)` (wave 44):
/// the receiver, slot 0 of an instance method's frame. A static method's
/// frame (a static native's included) is `INVALID_SLOT`, as HotSpot 25.0.3
/// answers before it looks at the frame; an instance native's or a compiled
/// frame is `OPAQUE_FRAME`.
pub(in crate::jvmti::native_env) extern "C" fn get_local_instance(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    depth: JInt,
    value_ptr: *mut JObject,
) -> JInt {
    let (shared, current, row) = match local_row(env, thread, depth, value_ptr.is_null()) {
        Ok((shared, LocalTarget::Current(current, row))) => (shared, current, row),
        Ok((shared, LocalTarget::Other(tid, depth))) => {
            return match other_local(&shared, tid, depth, LocalOp::Instance) {
                Ok(Some(LocalRead::Object(global))) => {
                    let handle = global_to_local(&shared, global);
                    // SAFETY: the agent's non-null out-parameter.
                    unsafe { *value_ptr = handle };
                    ERR_NONE
                }
                Ok(_) => ERR_INTERNAL,
                Err(code) => code,            };        }
        Err(code) => return code,
    };
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame (an instance
        // method of the spun class: the proxy).
        let _fx = jni::ForeignJniEntry::enter();
        // SAFETY: the calling thread's own `JvmThread`, inside this call.
        let receiver = match proxy_local_read(unsafe { &*current }, record, 0, LocalKind::Object) {
            Ok(Value::Object(receiver)) => receiver,
            Ok(_) => return ERR_TYPE_MISMATCH,
            Err(code) => return code,
        };
        let prev = jni::replace_jni_context(&shared);
        let handle = receiver.map_or(0, jni::new_local_handle);
        jni::restore_jni_context(prev);
        // SAFETY: the agent's non-null out-parameter.
        unsafe { *value_ptr = handle };
        return ERR_NONE;
    }
    let is_static = with_method(&shared, row.info.method, |_, m| m.is_static()).unwrap_or(true);
    if is_static {
        return ERR_INVALID_SLOT;
    }
    if let (None, Some(home)) = (row.frame, row.home) {
        // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd frame.
        let op = LocalOp::Instance;
        return match compiled_locals::current_home_read(&shared, current, &row.info, &home, op) {
            Ok(LocalRead::Object(handle)) => {
                // SAFETY: the agent's non-null out-parameter.
                unsafe { *value_ptr = handle };
                ERR_NONE
            }
            Ok(_) => ERR_INTERNAL,
            Err(code) => code,
        };
    }
    let Some(index) = row.frame else {
        return ERR_OPAQUE_FRAME;
    };
    let _fx = jni::ForeignJniEntry::enter();
    // SAFETY: as in `read_local`.
    let Some(frame) = (unsafe { (*current).frames.get(index) }) else {
        return ERR_OPAQUE_FRAME;
    };
    let crate::types::Value::Object(receiver) = crate::debug::debugger_local(frame, 0) else {
        return ERR_TYPE_MISMATCH;
    };
    let prev = jni::replace_jni_context(&shared);
    let handle = receiver.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *value_ptr = handle };
    ERR_NONE
}

/// Slot 11: `GetCurrentContendedMonitor(env, thread, monitor_ptr)` (wave
/// 44): the monitor the thread is blocked entering (never one it waits on,
/// JDK-8256314, unless it was notified and is blocked re-entering it), NULL
/// when none, from its lock record
/// (`ThreadRegistry::jmx_lock_snapshot`, which any thread's record answers).
/// The current thread (a NULL `thread`) is never blocked entering one.
pub(in crate::jvmti::native_env) extern "C" fn get_current_contended_monitor(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    monitor_ptr: *mut JObject,
) -> JInt {
    let shared = match live_phase_vm_with(env, 0, CAN_GET_CURRENT_CONTENDED_MONITOR) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Interpreter round i5 wave 101, lane L1: the out-parameter first, as
    // HotSpot's generated entry pre-checks it (see `get_thread_info`).
    if monitor_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let tid = if thread == 0 {
        let Some(current) = jni::current_jvm_thread_of(&shared) else {
            return ERR_UNATTACHED_THREAD;
        };
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        unsafe { (*current).thread_id.0 }
    } else {
        match thread_id_of(&shared, thread) {
            Ok(tid) => tid,
            Err(code) => return code,
        }
    };
    let _fx = jni::ForeignJniEntry::enter();
    // Interpreter round i2 wave 56, lanes L1 (`L1W56JvmtiReentryContended`)
    // and L6: a notified waiter re-entering its monitor -- BLOCKED while its
    // wait's monitor still stands -- contends for that monitor, as HotSpot
    // answers it (JDK-8256314) and as JDWP's twin answers since wave 55
    // (`ThreadRegistry::current_contended_monitor`). This answered NULL.
    let contended = shared
        .threads
        .thread_registry
        .current_contended_monitor(ThreadId(tid));
    let prev = jni::replace_jni_context(&shared);
    let handle = contended.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *monitor_ptr = handle };
    ERR_NONE
}

/// Slot 153: `GetOwnedMonitorStackDepthInfo(env, thread, count_ptr,
/// info_ptr)` (wave 44), of the current thread: the monitors it owns, in
/// the order and at the depths JDWP's `OwnedMonitorsStackDepthInfo`
/// answers them (`debug::inspect::jvmti_owned_monitor_order`: by frame,
/// innermost first, each frame's in the order it entered them; -1 for a
/// monitor no interpreter frame records, one entered through JNI), each
/// depth a place in this table's listing ([`listed_rows`]). HotSpot 25.0.3:
/// `count=1 depth=1` for a `synchronized` block of the frame below the
/// native that asks. Another thread's (interpreter round i1 wave 46, lane
/// L1; it was `JVMTI_ERROR_NOT_AVAILABLE`) are read where its frames hold
/// still ([`other_monitors`]), running or not, as HotSpot reads them through
/// a handshake.
pub(in crate::jvmti::native_env) extern "C" fn get_owned_monitor_stack_depth_info(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    count_ptr: *mut JInt,
    info_ptr: *mut *mut MonitorDepthRow,
) -> JInt {
    owned_monitor_rows(
        env,
        thread,
        (1, CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO),
        count_ptr,
        info_ptr,
    )
}

/// Slot 10: `GetOwnedMonitorInfo(env, thread, count_ptr, monitors_ptr)`
/// (interpreter round i1 wave 51, lane L1), behind
/// `can_get_owned_monitor_info`: the monitors
/// [`get_owned_monitor_stack_depth_info`] answers, in its order, without
/// their depths, as HotSpot builds both answers from one list
/// (`JvmtiEnvBase::get_owned_monitors`). It was `JVMTI_ERROR_NOT_AVAILABLE`.
/// The errors come in the same order (a NULL out-parameter is found where
/// that function finds it).
pub(in crate::jvmti::native_env) extern "C" fn get_owned_monitor_info(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    count_ptr: *mut JInt,
    monitors_ptr: *mut *mut JObject,
) -> JInt {
    let mut count: JInt = 0;
    let mut rows: *mut MonitorDepthRow = std::ptr::null_mut();
    let (inner_count, inner_rows) = if count_ptr.is_null() || monitors_ptr.is_null() {
        (std::ptr::null_mut(), std::ptr::null_mut())
    } else {
        (
            &mut count as *mut JInt,
            &mut rows as *mut *mut MonitorDepthRow,
        )
    };
    let code = owned_monitor_rows(
        env,
        thread,
        (0, CAN_GET_OWNED_MONITOR_INFO),
        inner_count,
        inner_rows,
    );
    if code != ERR_NONE {
        return code;
    }
    let n = usize::try_from(count).unwrap_or(0);
    let Some(size) = n.checked_mul(std::mem::size_of::<JObject>()) else {
        // SAFETY: `rows` is the block `owned_monitor_rows` just allocated.
        unsafe { free_block(rows.cast::<u8>()) };
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<JObject>();
    if block.is_null() {
        // SAFETY: as above.
        unsafe { free_block(rows.cast::<u8>()) };
        return ERR_OUT_OF_MEMORY;
    }
    for i in 0..n {
        // SAFETY: `rows` holds `count` rows and `block` room for as many
        // handles (`allocate_block` of their size, 16-aligned).
        unsafe { block.add(i).write((*rows.add(i)).monitor) };
    }
    // SAFETY: `rows` is the block `owned_monitor_rows` allocated; the agent's
    // out-parameters are non-null (checked above).
    unsafe {
        free_block(rows.cast::<u8>());
        *count_ptr = count;
        *monitors_ptr = block;
    }
    ERR_NONE
}

/// The monitors `owned` in `GetOwnedMonitorStackDepthInfo`'s order, each at
/// the depth of the row (`rows`, [`listed_rows`] of the CALLING thread, whose
/// `frames` these are) that locked it.
///
/// JIT round 19 proposal wave 7, lane gc7 (SYNC619-3): with compiled
/// activations on the calling thread's stack, attributed by TRACE index --
/// the interpreter frames' monitors and the compiled activations' own holds
/// (`held_monitors::frame_locked_monitors_with_compiled`, which checks its
/// layout against this trace entry by entry and answers `None` on any
/// disagreement) -- each at the depth of the row listing that trace entry
/// (`ListedRow::at`). Otherwise, or on `None`: by interpreter frame, as
/// before (a compiled hold at `-1`).
pub(in crate::jvmti::native_env) fn own_monitor_order(
    shared: &SharedVm,
    frames: &[crate::runtime::frame::Frame],
    rows: &[ListedRow],
    owned: &[ObjectRef],
) -> Vec<(ObjectRef, JInt)> {
    use crate::runtime::interpreter::held_monitors as hm;
    let traced = if crate::jit::conservative_roots::current_thread_jit_depth() != 0
        && hm::compiled_frame_monitor_depths_enabled()
        && hm::jvmti_compiled_frame_monitors_enabled()
    {
        let (trace, _) =
            crate::runtime::stackwalker::capture_trace_with_anchor_positions(shared, frames, &[]);
        hm::frame_locked_monitors_with_compiled(shared, frames, &trace, trace.len(), &[])
    } else {
        None
    };
    match traced {
        Some(pairs) => crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |at| {
            rows.iter()
                .position(|row| row.at == Some(at))
                .and_then(|depth| JInt::try_from(depth).ok())
        }),
        None => {
            let pairs = hm::frame_locked_monitors(frames);
            crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |index| {
                rows.iter()
                    .position(|row| row.frame == Some(index))
                    .and_then(|depth| JInt::try_from(depth).ok())
            })
        }
    }
}

/// [`get_owned_monitor_stack_depth_info`]'s body, behind the capability
/// `(word, bit)` the asking function needs.
pub(in crate::jvmti::native_env) fn owned_monitor_rows(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    (word, bit): (usize, u32),
    count_ptr: *mut JInt,
    info_ptr: *mut *mut MonitorDepthRow,
) -> JInt {
    let shared = match live_phase_vm_with(env, word, bit) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Interpreter round i5 wave 101, lane L1: the out-parameters first, as
    // HotSpot's generated entry pre-checks them (see `get_thread_info`).
    if count_ptr.is_null() || info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
    let current_tid = unsafe { (*current).thread_id.0 };
    let other = if thread == 0 {
        None
    } else {
        match thread_id_of(&shared, thread) {
            Ok(tid) => Some(tid).filter(|&tid| tid != current_tid),
            Err(code) => return code,
        }
    };
    if let Some(tid) = other {
        // Wave 46 (lane L1): read where the thread holds still, as HotSpot
        // reads a running thread's through a handshake.
        return match other_monitors(&shared, tid) {
            Ok(monitors) => write_monitor_rows(&shared, &monitors, count_ptr, info_ptr),
            Err(code) => code,
        };
    }
    // SAFETY: the calling thread's own `JvmThread`, inside this call.
    let rows = listed_rows(&shared, unsafe { &*current }, true);
    let _fx = jni::ForeignJniEntry::enter();
    let Some((contended, waiting, locked, ..)) = shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(ThreadId(current_tid))
    else {
        return ERR_INTERNAL;
    };
    let owned: Vec<ObjectRef> = locked
        .into_iter()
        .filter(|&o| Some(o) != waiting && Some(o) != contended)
        .collect();
    // SAFETY: as above.
    let monitors = own_monitor_order(&shared, unsafe { &(*current).frames }, &rows, &owned);
    let Ok(count) = JInt::try_from(monitors.len()) else {
        return ERR_INTERNAL;
    };
    let Some(size) = monitors
        .len()
        .checked_mul(std::mem::size_of::<MonitorDepthRow>())
    else {
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<MonitorDepthRow>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    let prev = jni::replace_jni_context(&shared);
    for (i, &(obj, depth)) in monitors.iter().enumerate() {
        let row = MonitorDepthRow {
            monitor: jni::new_local_handle(obj),
            stack_depth: depth,
        };
        // SAFETY: `block` holds `monitors.len()` rows (`allocate_block` of
        // their size, 16-aligned).
        unsafe { block.add(i).write(row) };
    }
    jni::restore_jni_context(prev);
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = count;
        *info_ptr = block;
    }
    ERR_NONE
}

// ---------------------------------------------------------------------------
// Another thread's locals and monitors (interpreter round i1 wave 46, lane L1)
// ---------------------------------------------------------------------------
//
// What remained of
// `docs/feature-designs/jvmti-delivery-threading.md`:
// the local-variable functions of another (suspended) thread and
// `GetOwnedMonitorStackDepthInfo` of another (any) thread. Both are answered
// where the thread's frames hold still, by the same two routes the stack
// functions use ([`other_thread_frames`]):
//
// * blocked in a native region: through its inspection window
//   (`interpreter::with_blocked_frames`), read by the caller while the
//   thread's leave waits. A reference local is its current address from the
//   blocking deposit's per-slot record; a primitive may be written in place
//   (the wake rewrites only reference slots); a reference is not (it would
//   be neither a root nor remapped while the thread sleeps): since wave 47
//   it is recorded, held by a global reference, for the thread's wake to
//   store after its write-back (`debug::inspect::defer_jvmti_object_write`),
//   and a read before the wake answers it; wave 46 answered
//   `JVMTI_ERROR_NOT_AVAILABLE`.
// * parked at an interpreter suspend point: on the thread itself
//   (`debug::run_on_parked_thread`), which is a registered mutator between
//   two polls, while the caller waits GC-safe. An object crosses between the
//   two threads as a JNI global reference ([`global_ref_of`],
//   [`global_to_local`]), so a collection between the answer and the
//   caller's local handle cannot move it unseen.
//
// Depths count the rows [`frames_of`] answers for the thread
// ([`other_rows`]); a row's interpreter frame is checked against the frame
// before it is touched. Each code is HotSpot 25.0.3's, measured
// (`tools/probes/interp/L1/L1W46JvmtiOtherThreadLocalsAndMonitors.java`).
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[JVMTI_OTHER_LOCAL]
// tid=<n> depth=<d> via=window|parked err=<code>` per local read or write and
// `[JVMTI_OTHER_MONITORS] tid=<n> count=<k> via=window|parked
// handshake=<bool>` per monitor read.

/// A local-variable function's operation on another thread's frame.
#[derive(Clone, Copy)]
pub(in crate::jvmti::native_env) enum LocalOp {
    /// Read local `slot` (the agent's `jint`) as `kind`.
    Read(LocalKind, JInt),
    /// Write a `kind` value into local `slot`; an object value is a JNI
    /// global reference (0: null) the caller made ([`write_other_local`]).
    Write(LocalKind, JInt, LocalRead),
    /// `GetLocalInstance`: the receiver.
    Instance,
}

impl LocalOp {
    pub(in crate::jvmti::native_env) fn name(self) -> &'static str {
        match self {
            LocalOp::Read(..) => "read",
            LocalOp::Write(..) => "write",
            LocalOp::Instance => "instance",
        }
    }
}

/// Run `op` on the frame at `depth` of suspended thread `tid`, another than
/// the caller, where its frames hold still ([`on_stopped_thread`]). An
/// object read comes back as a JNI global reference, which
/// [`caller_value`] makes the caller's; a write answers `None`.
pub(in crate::jvmti::native_env) fn other_local(
    shared: &SharedVm,
    tid: u64,
    depth: usize,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let answer = on_stopped_thread(
        shared,
        tid,
        move |shared: &SharedVm, thread: &mut JvmThread| {
            local_op_on_parked(shared, thread, depth, op)
        },
        |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
            local_op_through_window(shared, held, depth, op)
        },
    );
    let (answer, via) = match answer {
        Ok((answer, via)) => (answer, via),
        Err(code) => (Err(code), "none"),
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_OTHER_LOCAL] tid={tid} depth={depth} op={} via={via} err={}",
            op.name(),
            answer.as_ref().err().copied().unwrap_or(ERR_NONE)
        );
    }
    answer
}

/// A write of `value` (of `kind`) into local `slot` of the frame at `depth`
/// of suspended thread `tid` ([`other_local`]): an object `value`, the
/// agent's handle (0: null), is carried as a global reference, deleted once
/// the write is done.
pub(in crate::jvmti::native_env) fn write_other_local(
    shared: &SharedVm,
    tid: u64,
    depth: usize,
    slot: JInt,
    kind: LocalKind,
    value: LocalRead,
) -> JInt {
    let carried = match value {
        LocalRead::Object(handle) => match local_to_global(shared, handle) {
            Ok(global) => LocalRead::Object(global),
            Err(code) => return code,
        },
        other => other,
    };
    let answer = other_local(shared, tid, depth, LocalOp::Write(kind, slot, carried));
    if let LocalRead::Object(global) = carried {
        release_global(shared, global);
    }
    match answer {
        Ok(_) => ERR_NONE,
        Err(code) => code,
    }
}

/// Run `parked` on thread `tid` while it is parked at an interpreter suspend
/// point, or `window` on its frames while it is blocked in a native region,
/// whichever it is in first, waiting up to [`STOP_WAIT`] for either (a
/// suspended thread in a compiled loop stops at its next exit-capable poll).
/// Answers the work's answer and the route (`"parked"` / `"window"`);
/// `JVMTI_ERROR_THREAD_NOT_ALIVE` when the thread ended,
/// `JVMTI_ERROR_NOT_AVAILABLE` when it did not stand still in time. The
/// caller holds no lock; it waits GC-safe.
pub(in crate::jvmti::native_env) fn on_stopped_thread<R, P, W>(
    shared: &SharedVm,
    tid: u64,
    parked: P,
    window: W,
) -> Result<(R, &'static str), JInt>
where
    R: Send + 'static,
    P: FnOnce(&SharedVm, &mut JvmThread) -> R + Send + Clone + 'static,
    W: Fn(&crate::runtime::interpreter::BlockedFrames<'_>) -> R,
{
    let deadline = std::time::Instant::now() + STOP_WAIT;
    loop {
        if let Some(answer) = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window)
        {
            return Ok((answer, "window"));
        }
        // An unmounted virtual thread (a continuation that yielded in a sleep
        // or a park) has neither a window nor a carrier to park on: its
        // deposited frames are read where they lie (interpreter round i1
        // wave 51, lane L1;
        // `interpreter-L1-the-c-jvmti-table-reads-no-locals-of-an-unmounted-virtual-thread-FIXED-20261015`).
        if let Some(answer) =
            crate::runtime::interpreter::with_unmounted_frames(shared, tid, &window)
        {
            return Ok((answer, "unmounted"));
        }
        if shared.debug.debug_state.lock().is_parked(tid) {
            let work = parked.clone();
            let ran = gc_safe(shared, || {
                crate::debug::run_on_parked_thread(shared, Some(tid), work)
            });
            match ran {
                Ok(answer) => return Ok((answer, "parked")),
                Err(crate::debug::ParkedError::Lost) => return Err(ERR_INTERNAL),
                // Busy (another request's work), or it left its park: again.
                Err(_) => {}
            }
        }
        if !shared.threads.thread_registry.is_alive(ThreadId(tid)) {
            return Err(ERR_THREAD_NOT_ALIVE);
        }
        if std::time::Instant::now() >= deadline {
            return Err(ERR_NOT_AVAILABLE);
        }
        gc_safe(shared, || {
            std::thread::sleep(std::time::Duration::from_millis(1))
        });
    }
}

/// Is row `depth` of a blocked thread's `rows` (top first) a blocking
/// stand-in's `Object.wait()V` / `Object.wait(J)V` Java level -- a row with
/// no interpreter frame and no compiled activation, above the thread's first
/// interpreter frame -- and if so, its `long` timeout when known (`Some(0)`
/// for `wait(J)V` called by `wait()V`, the row below it)? Interpreter round
/// i6 wave 121, lane L1 (i119-L1 step 4). Above the first interpreter frame,
/// every `Object.wait` row is a level of the wait the thread is blocked in,
/// so its receiver is the monitor it waits on. (An inlined `wait` level of a
/// compiled caller qualifies too, and has the same receiver.)
pub(in crate::jvmti::native_env) fn waited_monitor_level(shared: &SharedVm, rows: &[OtherRow], depth: usize) -> Option<Option<i64>> {
    if rows.get(..=depth)?.iter().any(|r| r.raw.frame.is_some()) {
        return None;
    }
    let is_wait = |row: &OtherRow, descriptor: &str| -> bool {
        if row.raw.frame.is_some() || row.raw.chain.is_some() || row.info.method == 0 {
            return false;
        }
        // Cast: the id's halves, as `with_method` reads them.
        let class_id = ClassId::new((row.info.method >> 32) as u32);
        let index = (row.info.method & 0xFFFF) as usize;
        let cm = shared.classes.class_manager.read();
        cm.get_class(class_id).is_some_and(|class| {
            &*class.name == "java/lang/Object"
                && class
                    .methods
                    .get(index)
                    .is_some_and(|m| &*m.name == "wait" && &*m.descriptor == descriptor)
        })
    };
    let row = rows.get(depth)?;
    if is_wait(row, "()V") {
        return Some(None);
    }
    if is_wait(row, "(J)V") {
        let under_wait = rows
            .get(depth + 1)
            .is_some_and(|below| is_wait(below, "()V"));
        return Some(under_wait.then_some(0));
    }
    None
}

/// `op` on a stand-in's `Object.wait` level of blocked thread `tid`
/// ([`waited_monitor_level`]): the receiver (`GetLocalInstance`, or slot 0 as
/// an object) is the monitor the thread waits on, read from the slot a moving
/// collection forwards (`ThreadRegistry::peek_jmx_waiting_monitor`) and made
/// global inside the foreign-thread entry; slot 1 as a `long` is the timeout
/// when known. Anything else, a write included (the level has no frame to
/// write), and a thread that published no monitor: `OPAQUE_FRAME`, as
/// before.
pub(in crate::jvmti::native_env) fn waited_level_local(
    shared: &SharedVm,
    tid: u64,
    timeout: Option<i64>,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    match op {
        LocalOp::Instance | LocalOp::Read(LocalKind::Object, 0) => {
            let _fx = jni::ForeignJniEntry::enter();
            let monitor = shared
                .threads
                .thread_registry
                .peek_jmx_waiting_monitor(ThreadId(tid))
                .ok_or(ERR_OPAQUE_FRAME)?;
            Ok(Some(LocalRead::Object(global_ref_of(
                shared,
                Some(monitor),
            ))))
        }
        LocalOp::Read(LocalKind::Long, 1) => timeout
            .map(|t| Some(LocalRead::Long(t)))
            .ok_or(ERR_OPAQUE_FRAME),
        _ => Err(ERR_OPAQUE_FRAME),
    }
}

/// `op` on a `VirtualThread` instance row of unmounted bound virtual thread
/// `tid`'s continuation head (`StandinLevelLocals::VirtualThreadThis`;
/// interpreter round i6 wave 132, lane L1, the i119-L1 residual): the
/// receiver (`GetLocalInstance`, or slot 0 as an object) is the virtual
/// thread itself, `ThreadRegistry::java_thread_obj` (a forwarded root), made
/// global inside the foreign-thread entry -- the object JDWP's `ThisObject`
/// answers there (`publish_frame_snapshot`, wave 129) and HotSpot 25.0.3's
/// `ThisObject` answers (`L1W129RawJdwpVirtualWaitThisObject`). Anything
/// else, a write included (an unmounted thread's frames are read only, as
/// HotSpot's stack chunk is), and a thread with no mirror: `OPAQUE_FRAME`,
/// as before.
pub(in crate::jvmti::native_env) fn virtual_thread_level_local(
    shared: &SharedVm,
    tid: u64,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    match op {
        LocalOp::Instance | LocalOp::Read(LocalKind::Object, 0) => {
            let _fx = jni::ForeignJniEntry::enter();
            let thread = shared
                .threads
                .thread_registry
                .java_thread_obj(ThreadId(tid))
                .ok_or(ERR_OPAQUE_FRAME)?;
            Ok(Some(LocalRead::Object(global_ref_of(shared, Some(thread)))))
        }
        _ => Err(ERR_OPAQUE_FRAME),
    }
}

/// The interpreter frame whose locals `row` names: `OPAQUE_FRAME` for a
/// native method's row, a compiled activation, and a frame whose body runs
/// compiled (its locals are its compiled entry's), as for the current thread.
pub(in crate::jvmti::native_env) fn local_frame_index(row: &OtherRow) -> Result<usize, JInt> {
    row.raw
        .frame
        .filter(|_| !row.raw.runs_compiled)
        .ok_or(ERR_OPAQUE_FRAME)
}

/// Does `frame` run the method `raw` lists (the listing describes these
/// frames)?
pub(in crate::jvmti::native_env) fn row_names_frame(raw: &BlockedRow, frame: &crate::runtime::frame::Frame) -> bool {
    u64::from(frame.class_id.as_u32()) == raw.class_id
        && crate::debug::frame_method_id(frame) == raw.method_id
}

/// `GetLocalInstance` of a row whose method is static: `INVALID_SLOT`, as
/// HotSpot 25.0.3 answers before it looks at the frame.
pub(in crate::jvmti::native_env) fn instance_of_static(shared: &SharedVm, row: &OtherRow) -> bool {
    with_method(shared, row.info.method, |_, m| m.is_static()).unwrap_or(true)
}

/// [`other_local`] on the parked thread itself: `thread` runs this, between
/// two polls, and lists its own frames as the current thread's are listed
/// ([`listed_rows`]: the natives it runs, a JNI native in the middle of its
/// stack included, and the frames whose bodies run compiled), so a depth
/// names the frame [`frames_of`] lists there.
pub(in crate::jvmti::native_env) fn local_op_on_parked(
    shared: &SharedVm,
    thread: &mut JvmThread,
    depth: usize,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let rows = listed_rows(shared, thread, true);
    let row = rows.get(depth).ok_or(ERR_NO_MORE_FRAMES)?;
    if let (None, Some(record)) = (row.frame, row.proxy_locals) {
        // The i54-L1 retirement: a lambda proxy method's frame.
        return match op {
            LocalOp::Instance => match proxy_local_read(thread, record, 0, LocalKind::Object)? {
                Value::Object(receiver) => {
                    Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
                }
                _ => Err(ERR_TYPE_MISMATCH),
            },
            LocalOp::Read(kind, slot) => {
                let value = proxy_local_read(thread, record, slot, kind)?;
                local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
            }
            LocalOp::Write(kind, slot, value) => {
                let value = match value {
                    LocalRead::Int(v) => Value::Int(v),
                    LocalRead::Long(v) => Value::Long(v),
                    LocalRead::Float(v) => Value::Float(v),
                    LocalRead::Double(v) => Value::Double(v),
                    LocalRead::Object(global) => {
                        let obj = if global == 0 {
                            None
                        } else {
                            shared.natives.jni_global_refs.lock().resolve(global)
                        };
                        if global != 0 && obj.is_none() {
                            return Err(ERR_INVALID_OBJECT);
                        }
                        Value::Object(obj)
                    }
                };
                proxy_local_write(thread, record, slot, kind, value).map(|()| None)
            }
        };
    }
    if matches!(op, LocalOp::Instance)
        && with_method(shared, row.info.method, |_, m| m.is_static()).unwrap_or(true)
    {
        return Err(ERR_INVALID_SLOT);
    }
    if let (None, Some(home)) = (row.frame, row.home) {
        // j27 wave 5 (lane JVMTIFRAME): a compiled or OSR'd frame, read on
        // the parked thread itself (its own walk); a write stays opaque.
        return compiled_locals::parked_home_read(shared, thread, &row.info, &home, op);
    }
    let index = row.frame.ok_or(ERR_OPAQUE_FRAME)?;
    let frame = thread.frames.get(index).ok_or(ERR_OPAQUE_FRAME)?;
    match op {
        LocalOp::Instance => {
            let crate::types::Value::Object(receiver) = crate::debug::debugger_local(frame, 0)
            else {
                return Err(ERR_TYPE_MISMATCH);
            };
            Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
        }
        LocalOp::Read(kind, slot) => {
            let declared =
                u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s));
            let slot = checked_local(declared, frame, slot, kind)?;
            // gcf f2/f: a liveness-dead reference slot may hold a stale address.
            let live = frame.live_locals_mask_here();
            let value = crate::debug::debugger_local_for_read(shared, frame, slot, &live);
            local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
        }
        LocalOp::Write(kind, slot, value) => {
            let declared =
                u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s));
            let signature = (kind == LocalKind::Object)
                .then(|| declared_reference_signature(shared, frame, slot))
                .flatten();
            let slot = checked_local(declared, frame, slot, kind)?;
            let value = match value {
                LocalRead::Int(v) => crate::types::Value::Int(v),
                LocalRead::Long(v) => crate::types::Value::Long(v),
                LocalRead::Float(v) => crate::types::Value::Float(v),
                LocalRead::Double(v) => crate::types::Value::Double(v),
                LocalRead::Object(global) => {
                    let obj = if global == 0 {
                        None
                    } else {
                        shared.natives.jni_global_refs.lock().resolve(global)
                    };
                    if global != 0 && obj.is_none() {
                        return Err(ERR_INVALID_OBJECT);
                    }
                    // Wave 84 (lane L1): HotSpot's `check_slot_type_lvt`.
                    if obj.is_some_and(|o| !object_fits_declared(shared, signature.as_deref(), o)) {
                        return Err(ERR_TYPE_MISMATCH);
                    }
                    crate::types::Value::Object(obj)
                }
            };
            // The frame is not executing while its thread is parked; the
            // bytecode after the park reads the new value (as JDWP's
            // `StackFrame.SetValues` writes it, `debug::inspect`).
            thread
                .frames
                .get_mut(index)
                .ok_or(ERR_OPAQUE_FRAME)?
                .set_local(slot, value);
            Ok(None)
        }
    }
}

/// [`other_local`] through a blocked thread's inspection window: the caller
/// runs this while the thread's leave waits.
pub(in crate::jvmti::native_env) fn local_op_through_window(
    shared: &SharedVm,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
    depth: usize,
    op: LocalOp,
) -> Result<Option<LocalRead>, JInt> {
    let rows = other_rows(shared, held.rows.clone());
    let row = *rows.get(depth).ok_or(ERR_NO_MORE_FRAMES)?;
    if matches!(op, LocalOp::Instance) && instance_of_static(shared, &row) {
        return Err(ERR_INVALID_SLOT);
    }
    // Interpreter round i6 wave 121, lane L1 (i119-L1 step 4,
    // `CRATONVM_DEBUG_STANDIN_LEVEL_LOCALS`, default off): a blocking
    // stand-in's `Object.wait` level, which has no frame.
    if shared.debug.debugger_gates.standin_level_locals() {
        // Interpreter round i6 wave 132, lane L1 (i119-L1 residual): an
        // unmounted bound virtual thread's `VirtualThread` continuation row,
        // tagged by the listing that headed it
        // (`continuation_head_of_unmounted`, wave 129), as JDWP reads it.
        // Wave 133 (lane L9 review): the tags are by LISTING position, and
        // `other_rows` drops a row it cannot name, so `depth` names the same
        // row only while nothing at or above it was dropped (the kept rows
        // keep their order); otherwise the old answer.
        if row.raw.frame.is_none()
            && rows[..=depth].iter().zip(&held.rows).all(|(kept, listed)| {
                kept.raw.class_id == listed.class_id
                    && kept.raw.method_id == listed.method_id
                    && kept.raw.location == listed.location
            })
            && held.level_locals_at(depth)
                == crate::runtime::interpreter::StandinLevelLocals::VirtualThreadThis
        {
            return virtual_thread_level_local(shared, held.tid(), op);
        }
        if let Some(timeout) = waited_monitor_level(shared, &rows, depth) {
            return waited_level_local(shared, held.tid(), timeout, op);
        }
    }
    let index = local_frame_index(&row)?;
    let frame = held.frames().get(index).ok_or(ERR_OPAQUE_FRAME)?;
    if !row_names_frame(&row.raw, frame) {
        return Err(ERR_NOT_AVAILABLE);
    }
    // The class manager first (the declared kind), then the references,
    // read and made global inside the foreign-thread entry, so no collection
    // moves them meanwhile.
    let declared = match op {
        LocalOp::Read(_, slot) | LocalOp::Write(_, slot, _) => {
            u16::try_from(slot).map_or(Declared::NoTable, |s| declared_local(shared, frame, s))
        }
        LocalOp::Instance => Declared::NoTable,
    };
    let signature = match op {
        LocalOp::Write(LocalKind::Object, slot, _) => {
            declared_reference_signature(shared, frame, slot)
        }
        _ => None,
    };
    let _fx = jni::ForeignJniEntry::enter();
    let now = |slot: u16, v: crate::types::Value| match v {
        crate::types::Value::Object(Some(held_ref)) => {
            crate::types::Value::Object(held.current_reference(shared, index, slot, held_ref))
        }
        other => other,
    };
    match op {
        LocalOp::Instance => {
            let crate::types::Value::Object(receiver) =
                now(0, crate::debug::debugger_local(frame, 0))
            else {
                return Err(ERR_TYPE_MISMATCH);
            };
            Ok(Some(LocalRead::Object(global_ref_of(shared, receiver))))
        }
        LocalOp::Read(kind, slot) => {
            let slot = checked_local(declared, frame, slot, kind)?;
            // Wave 47: a reference written while the thread sleeps waits for
            // its wake; a read before then answers it, as HotSpot answers the
            // value just written.
            // gcf f2/f: a liveness-dead reference slot may hold a stale
            // address; screened after the window's remap.
            let value = crate::debug::inspect::deferred_local_value(
                shared,
                held.tid(),
                held.frames(),
                index,
                slot,
            )
            .unwrap_or_else(|| {
                let live = frame.live_locals_mask_here();
                crate::debug::screen_dead_local(
                    shared,
                    &live,
                    slot,
                    now(slot, crate::debug::debugger_local(frame, slot)),
                )
            });
            local_read_of(kind, value, |obj| global_ref_of(shared, obj)).map(Some)
        }
        LocalOp::Write(kind, slot, value) => {
            let slot = checked_local(declared, frame, slot, kind)?;
            // Wave 51 (lane L1): an unmounted virtual thread's frames lie in
            // HotSpot's heap stack chunk, which `SetLocal*` does not write:
            // `JVMTI_ERROR_OPAQUE_FRAME`, after the slot checks
            // (`VM_BaseGetOrSetLocal::doit`).
            if held.unmounted() {
                return Err(ERR_OPAQUE_FRAME);
            }
            let value = match value {
                LocalRead::Int(v) => crate::types::Value::Int(v),
                LocalRead::Long(v) => crate::types::Value::Long(v),
                LocalRead::Float(v) => crate::types::Value::Float(v),
                LocalRead::Double(v) => crate::types::Value::Double(v),
                // Wave 47 (lane L1; item 1 of
                // `docs/feature-designs/jvmti-delivery-threading.md`):
                // not stored into the sleeping thread's frame, where it would
                // be neither a root nor remapped (see the section note), but
                // recorded for the thread's wake, held by a global reference
                // of its own, as JDWP's `SetValues` of wave 41 is.
                LocalRead::Object(global) => {
                    let obj = if global == 0 {
                        None
                    } else {
                        Some(
                            shared
                                .natives
                                .jni_global_refs
                                .lock()
                                .resolve(global)
                                .ok_or(ERR_INVALID_OBJECT)?,
                        )
                    };
                    // Wave 84 (lane L1): HotSpot's `check_slot_type_lvt`.
                    if obj.is_some_and(|o| !object_fits_declared(shared, signature.as_deref(), o)) {
                        return Err(ERR_TYPE_MISMATCH);
                    }
                    let own = obj.map(|obj| global_ref_of(shared, Some(obj)));
                    crate::debug::inspect::defer_jvmti_object_write(
                        shared,
                        held.tid(),
                        index,
                        held.frames().len(),
                        row.raw.class_id,
                        row.raw.method_id,
                        slot,
                        own,
                    );
                    return Ok(None);
                }
            };
            if !held.set_primitive_local(index, slot, value) {
                return Err(ERR_OPAQUE_FRAME);
            }
            Ok(None)
        }
    }
}

/// `value`, read from a local of `kind`, as a [`LocalRead`]; an object is
/// made a handle by `handle`. `TYPE_MISMATCH` when the value is of another
/// kind.
pub(in crate::jvmti::native_env) fn local_read_of(
    kind: LocalKind,
    value: crate::types::Value,
    handle: impl FnOnce(Option<ObjectRef>) -> JObject,
) -> Result<LocalRead, JInt> {
    Ok(match (kind, value) {
        (LocalKind::Int, crate::types::Value::Int(v)) => LocalRead::Int(v),
        (LocalKind::Long, crate::types::Value::Long(v)) => LocalRead::Long(v),
        (LocalKind::Float, crate::types::Value::Float(v)) => LocalRead::Float(v),
        (LocalKind::Double, crate::types::Value::Double(v)) => LocalRead::Double(v),
        (LocalKind::Object, crate::types::Value::Object(obj)) => LocalRead::Object(handle(obj)),
        _ => return Err(ERR_TYPE_MISMATCH),
    })
}

/// A JNI global reference to `obj` (0 for null), made by a thread no
/// collection can overtake meanwhile (a parked thread between two polls, or
/// a caller inside its foreign-thread entry).
pub(in crate::jvmti::native_env) fn global_ref_of(shared: &SharedVm, obj: Option<ObjectRef>) -> JObject {
    obj.map_or(0, |obj| shared.natives.jni_global_refs.lock().add(obj))
}

/// The calling thread's local reference to what `global` (a reference
/// [`global_ref_of`] made; 0: null) names, `global` deleted.
pub(in crate::jvmti::native_env) fn global_to_local(shared: &SharedVm, global: JObject) -> JObject {
    if global == 0 {
        return 0;
    }
    let _fx = jni::ForeignJniEntry::enter();
    let obj = shared.natives.jni_global_refs.lock().resolve(global);
    let prev = jni::replace_jni_context(shared);
    let handle = obj.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    let _ = shared.natives.jni_global_refs.lock().remove(global);
    handle
}

/// [`LocalRead`] `value` for the caller: an object's global reference made
/// its local one ([`global_to_local`]).
pub(in crate::jvmti::native_env) fn caller_value(shared: &SharedVm, value: LocalRead) -> LocalRead {
    match value {
        LocalRead::Object(global) => LocalRead::Object(global_to_local(shared, global)),
        other => other,
    }
}

/// A global reference to what the agent's `handle` names (0: null), for a
/// write on another thread; `INVALID_OBJECT` for a handle that names none.
pub(in crate::jvmti::native_env) fn local_to_global(shared: &SharedVm, handle: JObject) -> Result<JObject, JInt> {
    if handle == 0 {
        return Ok(0);
    }
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(handle);
    jni::restore_jni_context(prev);
    obj.map(|obj| shared.natives.jni_global_refs.lock().add(obj))
        .ok_or(ERR_INVALID_OBJECT)
}

/// Delete a global reference [`local_to_global`] made (0: none).
pub(in crate::jvmti::native_env) fn release_global(shared: &SharedVm, global: JObject) {
    if global != 0 {
        let _ = shared.natives.jni_global_refs.lock().remove(global);
    }
}

/// The monitors thread `tid` (another than the caller) owns, in
/// `GetOwnedMonitorStackDepthInfo`'s order and at its depths, each as a
/// JNI global reference ([`global_to_local`] makes it the caller's): read
/// through its window when it is blocked, else on the thread parked at a
/// suspend point, after a handshake of its own when it is not suspended.
pub(in crate::jvmti::native_env) fn other_monitors(shared: &SharedVm, tid: u64) -> Result<Vec<(JObject, JInt)>, JInt> {
    let window = |held: &crate::runtime::interpreter::BlockedFrames<'_>| {
        monitors_through_window(shared, tid, held)
    };
    if let Some(answer) = crate::runtime::interpreter::with_blocked_frames(shared, tid, &window) {
        return finish_monitors(tid, answer, "window", false);
    }
    // Wave 51 (lane L1): an unmounted virtual thread, read where it lies, as
    // its listing is (`other_thread_frames`); a handshake would wait in vain.
    if let Some(answer) = crate::runtime::interpreter::with_unmounted_frames(shared, tid, &window) {
        return finish_monitors(tid, answer, "unmounted", false);
    }
    let handshake = !shared.debug.debug_state.lock().is_thread_suspended(tid);
    if handshake {
        let mut ds = shared.debug.debug_state.lock();
        ds.begin_handshake(tid);
        crate::debug::publish_debugger_gates(shared, &ds);
    }
    let answer = on_stopped_thread(
        shared,
        tid,
        |shared: &SharedVm, thread: &mut JvmThread| monitors_on_parked(shared, thread),
        window,
    );
    if handshake {
        let mut ds = shared.debug.debug_state.lock();
        ds.end_handshake(tid);
        crate::debug::publish_debugger_gates(shared, &ds);
    }
    let (answer, via) = answer?;
    finish_monitors(tid, answer, via, handshake)
}

/// [`other_monitors`]' answer, traced.
pub(in crate::jvmti::native_env) fn finish_monitors(
    tid: u64,
    answer: Result<Vec<(JObject, JInt)>, JInt>,
    via: &str,
    handshake: bool,
) -> Result<Vec<(JObject, JInt)>, JInt> {
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_OTHER_MONITORS] tid={tid} count={} via={via} handshake={handshake}",
            answer
                .as_ref()
                .map_or(-1, |m| i64::try_from(m.len()).unwrap_or(-1))
        );
    }
    answer
}

/// The monitors `owned` in `GetOwnedMonitorStackDepthInfo`'s order, each at
/// the depth of the row whose interpreter frame locked it (`pairs`,
/// `held_monitors::frame_locked_monitors` of the frames, indexed from the
/// bottom), as global references.
pub(in crate::jvmti::native_env) fn monitor_rows_of(
    shared: &SharedVm,
    rows: &[OtherRow],
    owned: &[ObjectRef],
    pairs: Vec<(usize, ObjectRef)>,
) -> Vec<(JObject, JInt)> {
    let monitors = crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |index| {
        rows.iter()
            .position(|row| row.raw.frame == Some(index))
            .and_then(|depth| JInt::try_from(depth).ok())
    });
    let mut refs = shared.natives.jni_global_refs.lock();
    monitors
        .into_iter()
        .map(|(obj, depth)| (refs.add(obj), depth))
        .collect()
}

/// The monitors thread `tid` owns: its lock stack less the monitor it waits
/// on and the one it is blocked entering (neither is owned), as the current
/// thread's are ([`get_owned_monitor_stack_depth_info`]).
pub(in crate::jvmti::native_env) fn owned_monitors(shared: &SharedVm, tid: u64) -> Result<Vec<ObjectRef>, JInt> {
    let (contended, waiting, locked, ..) = shared
        .threads
        .thread_registry
        .jmx_lock_snapshot(ThreadId(tid))
        .ok_or(ERR_INTERNAL)?;
    Ok(locked
        .into_iter()
        .filter(|&o| Some(o) != waiting && Some(o) != contended)
        .collect())
}

/// [`other_monitors`] on the parked thread itself, between two polls, its
/// depths places in its own listing ([`listed_rows`]), as the current
/// thread's are.
pub(in crate::jvmti::native_env) fn monitors_on_parked(
    shared: &SharedVm,
    thread: &mut JvmThread,
) -> Result<Vec<(JObject, JInt)>, JInt> {
    let rows = listed_rows(shared, thread, true);
    let owned = owned_monitors(shared, thread.thread_id.0)?;
    // SYNC619-3 (round 19 wave 7, lane gc7): this runs ON the parked thread,
    // so its compiled activations are the calling thread's own.
    let monitors = own_monitor_order(shared, &thread.frames, &rows, &owned);
    let mut refs = shared.natives.jni_global_refs.lock();
    Ok(monitors
        .into_iter()
        .map(|(obj, depth)| (refs.add(obj), depth))
        .collect())
}

/// [`other_monitors`] through blocked thread `tid`'s inspection window. The
/// frames' own monitor records are the addresses the monitors had when the
/// thread blocked, so the attribution the thread published as it blocked
/// (`ThreadRegistry::jmx_frame_monitors`, read from its lock stack, which
/// every collection updates) is taken first, as JDWP's
/// `OwnedMonitorsStackDepthInfo` takes it; the frames' records only when
/// every monitor they name is still one the thread owns at that address.
pub(in crate::jvmti::native_env) fn monitors_through_window(
    shared: &SharedVm,
    tid: u64,
    held: &crate::runtime::interpreter::BlockedFrames<'_>,
) -> Result<Vec<(JObject, JInt)>, JInt> {
    let rows = other_rows(shared, held.rows.clone());
    let _fx = jni::ForeignJniEntry::enter();
    let frames = held.frames();
    // An unmounted virtual thread (wave 51) may have no registry entry of
    // its own while no carrier runs it: the monitors its deposited frames
    // locked are then the ones it owns.
    let owned = match owned_monitors(shared, tid) {
        Err(_) if held.unmounted() => {
            crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames)
                .into_iter()
                .map(|(_, obj)| obj)
                .collect()
        }
        other => other?,
    };
    // GC719-2 (JIT round 19 proposal wave 8, lane gc8): compiled frames on
    // the blocked thread's stack -- the published trace's attribution, rows
    // matched to its entries by name.
    if let Some(monitors) = window_compiled_monitor_rows(shared, tid, &rows, &owned, frames.len()) {
        return Ok(monitors);
    }
    let pairs = shared
        .threads
        .thread_registry
        .jmx_frame_monitors(ThreadId(tid), frames.len())
        .unwrap_or_else(|| {
            let pairs = crate::runtime::interpreter::held_monitors::frame_locked_monitors(frames);
            if pairs.iter().all(|(_, obj)| owned.contains(obj)) {
                pairs
            } else {
                Vec::new()
            }
        });
    Ok(monitor_rows_of(shared, &rows, &owned, pairs))
}



/// GC719-2 (JIT round 19 proposal wave 8, lane gc8): the monitors blocked
/// thread `tid` owns (`owned`), each at the depth of the window row
/// (`rows`, top first) whose frame locked it, when the thread's published
/// trace interleaves compiled activations with its `frames_len` interpreter
/// frames.
///
/// The thread published, at the deposit it is blocked in, its trace
/// (`ThreadRegistry::frame_trace_of`, innermost first) and the attribution
/// indexed into that trace -- interpreter frames' monitors and, since wave 7
/// (SYNC619-2), the compiled activations' own holds
/// (`ThreadRegistry::jmx_frame_monitors` asked with the trace's length,
/// which it answers only for a publication of that length and an unchanged
/// lock stack). The window's listing is built from the same deposit's chain
/// copy, so the two describe one stack; [`window_rows_for_trace`] proves it
/// entry by entry (names, and at most a native heading the listing adds) and
/// maps each trace entry to its row. Any disagreement, a trace with no
/// compiled entry, the switch off, or no publication answers `None`, and the
/// caller keeps wave 7's interpreter-frame answer.
pub(in crate::jvmti::native_env) fn window_compiled_monitor_rows(
    shared: &SharedVm,
    tid: u64,
    rows: &[OtherRow],
    owned: &[ObjectRef],
    frames_len: usize,
) -> Option<Vec<(JObject, JInt)>> {
    use crate::runtime::interpreter::held_monitors as hm;
    if !hm::compiled_frame_monitor_depths_enabled() {
        return None;
    }
    let registry = &shared.threads.thread_registry;
    let trace = registry.frame_trace_of(ThreadId(tid));
    // A trace one-to-one with the interpreter frames has no compiled entry,
    // and `jmx_frame_monitors` would answer its frame-indexed table.
    if trace.is_empty() || trace.len() == frames_len {
        return None;
    }
    let pairs = registry.jmx_frame_monitors(ThreadId(tid), trace.len())?;
    let names: Vec<Option<(String, Arc<str>)>> = rows
        .iter()
        .map(|row| row_class_and_method(shared, row))
        .collect();
    let row_of = window_rows_for_trace(&names, rows, &trace)?;
    let n = trace.len();
    let monitors = crate::debug::inspect::jvmti_owned_monitor_order(owned, pairs, |index| {
        // `index` counts the trace from the bottom; `row_of` from the top.
        let from_top = n.checked_sub(index + 1)?;
        JInt::try_from(*row_of.get(from_top)?).ok()
    });
    let mut refs = shared.natives.jni_global_refs.lock();
    Some(
        monitors
            .into_iter()
            .map(|(obj, depth)| (refs.add(obj), depth))
            .collect(),
    )
}

/// The `(class name, dotted; method name)` of a window row, or `None` when
/// the row names no method of a loaded class (a lambda proxy's row, an
/// obsolete method): [`window_rows_for_trace`] then refuses the whole match.
pub(in crate::jvmti::native_env) fn row_class_and_method(shared: &SharedVm, row: &OtherRow) -> Option<(String, Arc<str>)> {
    let (class_id, name, _) = method_or_proxy_method(shared, row.info.method)?;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    Some((class.name.replace('/', "."), name))
}

/// GC719-2: the row (index into `rows`, top first) of each entry of `trace`
/// (innermost first), when the two list one stack: equal length, or the
/// listing one longer with a native method heading it (`location == -1`, no
/// interpreter frame), and every entry's class and method name equal to its
/// row's (`names`, [`row_class_and_method`] per row; class names compared
/// dotted). `None` on any disagreement -- never a shifted attribution.
pub(in crate::jvmti::native_env) fn window_rows_for_trace(
    names: &[Option<(String, Arc<str>)>],
    rows: &[OtherRow],
    trace: &[crate::native::registry::StackTraceEntry],
) -> Option<Vec<usize>> {
    let head = rows.len().checked_sub(trace.len())?;
    match head {
        0 => {}
        1 if rows[0].raw.location == -1 && rows[0].raw.frame.is_none() => {}
        _ => return None,
    }
    let mut out = Vec::with_capacity(trace.len());
    for (k, entry) in trace.iter().enumerate() {
        let at = k + head;
        let (class, method) = names.get(at)?.as_ref()?;
        if **method != *entry.method_name || *class != entry.class_name.replace('/', ".") {
            return None;
        }
        out.push(at);
    }
    Some(out)
}

/// Write `monitors` (global references and depths, [`other_monitors`]) to
/// `GetOwnedMonitorStackDepthInfo`'s out-parameters as the caller's local
/// references, in an `Allocate`d block.
pub(in crate::jvmti::native_env) fn write_monitor_rows(
    shared: &SharedVm,
    monitors: &[(JObject, JInt)],
    count_ptr: *mut JInt,
    info_ptr: *mut *mut MonitorDepthRow,
) -> JInt {
    let Ok(count) = JInt::try_from(monitors.len()) else {
        monitors
            .iter()
            .for_each(|&(global, _)| release_global(shared, global));
        return ERR_INTERNAL;
    };
    let Some(size) = monitors
        .len()
        .checked_mul(std::mem::size_of::<MonitorDepthRow>())
    else {
        monitors
            .iter()
            .for_each(|&(global, _)| release_global(shared, global));
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(size).cast::<MonitorDepthRow>();
    if block.is_null() {
        monitors
            .iter()
            .for_each(|&(global, _)| release_global(shared, global));
        return ERR_OUT_OF_MEMORY;
    }
    for (i, &(global, depth)) in monitors.iter().enumerate() {
        let row = MonitorDepthRow {
            monitor: global_to_local(shared, global),
            stack_depth: depth,
        };
        // SAFETY: `block` holds `monitors.len()` rows (`allocate_block` of
        // their size, 16-aligned).
        unsafe { block.add(i).write(row) };
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = count;
        *info_ptr = block;
    }
    ERR_NONE
}

/// Slot 88: `GetVersionNumber(env, version_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn get_version_number(env: *mut JvmtiNativeEnv, version_ptr: *mut JInt) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if version_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *version_ptr = JVMTI_VERSION_25 };
    ERR_NONE
}

/// Write a capability set to the agent's `jvmtiCapabilities*`.
pub(in crate::jvmti::native_env) fn write_capabilities(out: *mut [u32; CAPABILITY_WORDS], caps: [u32; CAPABILITY_WORDS]) -> JInt {
    if out.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null, 16-byte `jvmtiCapabilities`.
    unsafe { out.write_unaligned(caps) };
    ERR_NONE
}

