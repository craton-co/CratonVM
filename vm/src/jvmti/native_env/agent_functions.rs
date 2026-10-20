// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The C table's remaining thread, class-file and VM functions (interpreter
//! round i2 wave 63, lane L1; the i51-L1 page's "Next step (from wave 61)",
//! items 1 and 5), each with HotSpot 25's checks in its order (MEASURED,
//! `tools/probes/interp/L1/L1W63JvmtiAgentFunctions.java`):
//!
//! * 7 / 8 `StopThread` / `InterruptThread` (`can_signal_thread`), through
//!   JDWP's `ThreadReference.Stop` / `Interrupt`;
//! * 12 `RunAgentThread`: the agent's function runs on a new daemon thread
//!   in place of the `Thread`'s `run()` (`vm_exec`'s `thread_start`, which
//!   takes the body [`take_pending_agent_body`] leaves);
//! * 59 `GetObjectMonitorUsage` (`can_get_monitor_info`);
//! * 73 / 74 `SetNativeMethodPrefix(es)` (`can_set_native_method_prefix`),
//!   which the native linker strips (`vm_exec::prefixed_native_link`,
//!   [`all_native_method_prefixes`]);
//! * 90 `GetSourceDebugExtension` (`can_get_source_debug_extension`) and 146
//!   `GetConstantPool` (`can_get_constant_pool`), read from the class file
//!   the class was defined from, as JDWP reads the first;
//! * 118 / 119 `Suspend/ResumeAllVirtualThreads`;
//! * 120 / 121 `Set/GetJNIFunctionTable`;
//! * 150 `SetVerboseFlag`;
//! * 156 `SetHeapSamplingInterval`.

use super::*;

/// `JVMTI_ERROR_INVALID_PRIORITY`.
const ERR_INVALID_PRIORITY: JInt = 12;

/// The capability bit field `n`'s (word, bit), for [`live_phase_vm_with`].
const fn word_bit(n: usize) -> (usize, u32) {
    (n / 32, 1 << (n % 32))
}

/// [`live_phase_vm_with`] for capability bit field `n`.
fn live_with_cap(env: *mut JvmtiNativeEnv, n: usize) -> Result<Arc<SharedVm>, JInt> {
    let (word, bit) = word_bit(n);
    live_phase_vm_with(env, word, bit)
}

/// Does `env` hold capability bit field `n`?
fn env_has(env: &JvmtiNativeEnv, n: usize) -> bool {
    has_cap(&env.state.lock().capabilities, n)
}

// --- SetVerboseFlag, the JNI function table ---------------------------------

/// Slot 150: `SetVerboseFlag(env, flag, value)`, any phase:
/// `JVMTI_VERBOSE_GC` (1) and `JVMTI_VERBOSE_CLASS` (2) turn HotSpot's
/// `gc` / `class+load` + `class+unload` info output to standard output on
/// or off (`unified_logging::set_jvmti_verbose`); `JVMTI_VERBOSE_OTHER` (0)
/// and `JVMTI_VERBOSE_JNI` (4) are accepted (HotSpot's JNI flag prints the
/// natives it links, which this VM does not); any other flag is
/// `ILLEGAL_ARGUMENT` (MEASURED: 3, 8 and -1).
pub(super) extern "C" fn set_verbose_flag(env: *mut JvmtiNativeEnv, flag: JInt, value: u8) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    match flag {
        0 | 4 => ERR_NONE,
        1 | 2 => {
            crate::runtime::unified_logging::set_jvmti_verbose(flag == 1, value != 0);
            ERR_NONE
        }
        _ => ERR_ILLEGAL_ARGUMENT,
    }
}

/// Is the env's VM in the start or the live phase (the JNI table
/// functions' phases)?
fn start_or_live(env: *mut JvmtiNativeEnv) -> Result<(), JInt> {
    let env = live_env(env)?;
    let shared = env.vm().ok_or(ERR_WRONG_PHASE)?;
    let gates = &shared.debug.debugger_gates;
    if gates.jvmti_outside_live() && !gates.jvmti_start_phase() {
        return Err(ERR_WRONG_PHASE);
    }
    Ok(())
}

/// Slot 121: `GetJNIFunctionTable(env, function_table)`: a copy of the JNI
/// function table every `JNIEnv*` of the process points at, in an
/// `Allocate` block (MEASURED: equal to the `JNIEnv*`'s, and not the table
/// itself).
pub(super) extern "C" fn get_jni_function_table(
    env: *mut JvmtiNativeEnv,
    table_ptr: *mut *mut usize,
) -> JInt {
    if let Err(code) = start_or_live(env) {
        return code;
    }
    if table_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let count = jni::JNI_FUNCTION_COUNT;
    let block = allocate_block(count * std::mem::size_of::<usize>()).cast::<usize>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    let table = jni::jni_function_table();
    // SAFETY: the table and the block both hold `count` entries.
    unsafe {
        for i in 0..count {
            block.add(i).write(std::ptr::read_volatile(table.add(i)));
        }
        *table_ptr = block;
    }
    ERR_NONE
}

/// Slot 120: `SetJNIFunctionTable(env, function_table)`: the agent's table
/// copied over the process's one, so every `JNIEnv*`, on every thread and
/// in the calling native too, calls the agent's functions from now on, as
/// HotSpot copies it into its `jni_NativeInterface` (MEASURED: a replaced
/// `GetVersion` counts the next call, and restoring the saved copy stops
/// it).
pub(super) extern "C" fn set_jni_function_table(
    env: *mut JvmtiNativeEnv,
    function_table: *const usize,
) -> JInt {
    if let Err(code) = start_or_live(env) {
        return code;
    }
    if function_table.is_null() {
        return ERR_NULL_POINTER;
    }
    let table = jni::jni_function_table();
    // SAFETY: the agent's table is a `jniNativeInterface` (`count` entries),
    // and the process's table holds as many. Entry by entry, so a thread
    // reading one meanwhile reads a whole pointer.
    unsafe {
        for i in 0..jni::JNI_FUNCTION_COUNT {
            std::ptr::write_volatile(table.add(i), function_table.add(i).read_unaligned());
        }
    }
    ERR_NONE
}

// --- Class-file attributes ---------------------------------------------------

/// The class file stored class `class` was defined from, when this VM still
/// has it (`debug::defined_class_file`).
fn class_file_of(shared: &SharedVm, class: &crate::classloading::Class) -> Option<Vec<u8>> {
    let cm = shared.classes.class_manager.read();
    crate::debug::defined_class_file(&cm, class.id, &class.name)
}

/// Slot 90: `GetSourceDebugExtension(env, klass, source_debug_extension_ptr)`
/// (`can_get_source_debug_extension`): the `SourceDebugExtension` attribute
/// of the class file, as JDWP answers it; `ABSENT_INFORMATION` without one
/// and for an array, a primitive or a lambda proxy class (MEASURED: `int[]`
/// and `int` are 101).
pub(super) extern "C" fn get_source_debug_extension(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    sde_ptr: *mut *mut u8,
) -> JInt {
    match live_env(env) {
        Ok(e) if !env_has(e, CAP_GET_SOURCE_DEBUG_EXTENSION) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(_) => {}
        Err(code) => return code,
    }
    let smap = match with_jvmti_class(
        env,
        klass,
        |shared, class| {
            if class.name.starts_with('[') {
                return None;
            }
            class_file_of(shared, class)
                .as_deref()
                .and_then(crate::debug::source_debug_extension_of)
        },
        |_, _| None,
        |_| None,
    ) {
        Ok(smap) => smap,
        Err(code) => return code,
    };
    if sde_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(smap) = smap else {
        return ERR_ABSENT_INFORMATION;
    };
    let block = allocate_block(smap.len() + 1);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: a fresh block of `smap.len() + 1` bytes; the agent's non-null
    // out-parameter. The attribute is modified UTF-8 already.
    unsafe {
        std::ptr::copy_nonoverlapping(smap.as_ptr(), block, smap.len());
        block.add(smap.len()).write(0);
        *sde_ptr = block;
    }
    ERR_NONE
}

/// Slot 146: `GetConstantPool(env, klass, constant_pool_count_ptr,
/// constant_pool_byte_count_ptr, constant_pool_bytes_ptr)`
/// (`can_get_constant_pool`; live phase): the constant pool of the class
/// file the class was defined from, as HotSpot reconstitutes it (MEASURED:
/// the count and the bytes of the class file's own pool for a plain class);
/// a lambda proxy class's, as its spun class holds it (the i54-L1 retirement);
/// `ABSENT_INFORMATION` for an array or a primitive class (MEASURED 101) and
/// where this VM no longer has the class file.
pub(super) extern "C" fn get_constant_pool(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    count_ptr: *mut JInt,
    byte_count_ptr: *mut JInt,
    bytes_ptr: *mut *mut u8,
) -> JInt {
    if let Err(code) = live_with_cap(env, CAP_GET_CONSTANT_POOL) {
        return code;
    }
    let answer = match with_jvmti_class(
        env,
        klass,
        |shared, class| {
            if class.name.starts_with('[') {
                return None;
            }
            let file = class_file_of(shared, class)?;
            crate::debug::class_file_constant_pool(&file)
                .map(|(count, pool)| (count, pool.to_vec()))
        },
        // The i54-L1 retirement: a lambda proxy class's pool, as the class
        // `InnerClassLambdaMetafactory` spins holds it once HotSpot defines
        // it hidden (`runtime::lambda_proxy::lambda_proxy_spun_class_of`).
        |shared, id| {
            crate::runtime::lambda_proxy::lambda_proxy_spun_class_of(shared, id)
                .map(|spun| spun.constant_pool())
        },
        |_| None,
    ) {
        Ok(answer) => answer,
        Err(code) => return code,
    };
    if count_ptr.is_null() || byte_count_ptr.is_null() || bytes_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some((count, pool)) = answer else {
        return ERR_ABSENT_INFORMATION;
    };
    let Ok(len) = JInt::try_from(pool.len()) else {
        return ERR_INTERNAL;
    };
    let block = allocate_block(pool.len().max(1));
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: a fresh block of the pool's size; the agent's non-null
    // out-parameters.
    unsafe {
        std::ptr::copy_nonoverlapping(pool.as_ptr(), block, pool.len());
        *count_ptr = JInt::from(count);
        *byte_count_ptr = len;
        *bytes_ptr = block;
    }
    ERR_NONE
}

// --- GetObjectMonitorUsage ---------------------------------------------------

/// `jvmtiMonitorUsage`.
#[repr(C)]
pub(super) struct MonitorUsageRow {
    owner: JObject,
    entry_count: JInt,
    waiter_count: JInt,
    waiters: *mut JObject,
    notify_waiter_count: JInt,
    notify_waiters: *mut JObject,
}

/// Slot 59: `GetObjectMonitorUsage(env, object, info_ptr)`
/// (`can_get_monitor_info`), with JDK 23's meaning of the two lists
/// (MEASURED): `waiters` are the threads waiting to own the monitor -- the
/// ones blocked entering it and the notified waiters re-entering it --
/// `notify_waiters` the threads in `Object.wait` not notified yet; the owner
/// and its entry count. `INVALID_OBJECT` for NULL.
pub(super) extern "C" fn get_object_monitor_usage(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    info_ptr: *mut MonitorUsageRow,
) -> JInt {
    let shared = match live_with_cap(env, CAP_GET_MONITOR_INFO) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if object == 0 {
        return ERR_INVALID_OBJECT;
    }
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(&shared);
    let obj = jni::jobject_to_obj(object);
    let Some(obj) = obj else {
        jni::restore_jni_context(prev);
        return ERR_INVALID_OBJECT;
    };
    // Interpreter round i5 wave 111, lane L1: one snapshot of the owner, its
    // count and the wait set, shared with JDWP `MonitorInfo`
    // (`debug::inspect::monitor_usage`); the waiters in thread-list order,
    // the notify waiters in wait-set order (they were in registry-map order).
    let registry = &shared.threads.thread_registry;
    let usage = crate::debug::inspect::monitor_usage(&shared, obj);
    let (owner, entry_count, entering, waiting) = (
        usage.owner,
        usage.entry_count,
        usage.entering,
        usage.waiting,
    );
    let handle_of = |tid: ThreadId| {
        registry
            .java_thread_obj(tid)
            .map_or(0, jni::new_local_handle)
    };
    let owner_handle = owner.map_or(0, handle_of);
    let entering: Vec<JObject> = entering.into_iter().map(handle_of).collect();
    let waiting: Vec<JObject> = waiting.into_iter().map(handle_of).collect();
    jni::restore_jni_context(prev);
    let mut row = MonitorUsageRow {
        owner: owner_handle,
        entry_count: JInt::try_from(entry_count).unwrap_or(JInt::MAX),
        waiter_count: 0,
        waiters: std::ptr::null_mut(),
        notify_waiter_count: 0,
        notify_waiters: std::ptr::null_mut(),
    };
    let code = write_id_array(&mut row.waiter_count, &mut row.waiters, &entering);
    if code != ERR_NONE {
        return code;
    }
    let code = write_id_array(
        &mut row.notify_waiter_count,
        &mut row.notify_waiters,
        &waiting,
    );
    if code != ERR_NONE {
        return code;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { info_ptr.write(row) };
    ERR_NONE
}

// --- InterruptThread, StopThread -----------------------------------------------

/// The live thread a signal names: `(tid, its Thread)`. NULL is the calling
/// thread (MEASURED: `InterruptThread(NULL)` is 0); a `Thread` not started
/// or terminated `THREAD_NOT_ALIVE`; anything else `INVALID_THREAD`.
fn signal_target(shared: &SharedVm, thread: JObject) -> Result<(u64, ObjectRef), JInt> {
    let tid = if thread == 0 {
        let current = jni::current_jvm_thread_of(shared).ok_or(ERR_UNATTACHED_THREAD)?;
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        unsafe { (*current).thread_id.0 }
    } else {
        match lookup_thread(shared, thread)? {
            ThreadLookup::Live(tid) => tid,
            ThreadLookup::Dead | ThreadLookup::Unstarted => return Err(ERR_THREAD_NOT_ALIVE),
        }
    };
    let obj = shared
        .threads
        .thread_registry
        .java_thread_obj(ThreadId(tid))
        .ok_or(ERR_THREAD_NOT_ALIVE)?;
    Ok((tid, obj))
}

/// Slot 8: `InterruptThread(env, thread)` (`can_signal_thread`): what
/// `Thread.interrupt()` does, as `ThreadReference.Interrupt` does it
/// (`debug::inspect::jvmti_interrupt`; MEASURED: a sleeping platform or
/// virtual thread throws `InterruptedException`).
pub(super) extern "C" fn interrupt_thread(env: *mut JvmtiNativeEnv, thread: JObject) -> JInt {
    let shared = match live_with_cap(env, CAP_SIGNAL_THREAD) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    let _fx = jni::ForeignJniEntry::enter();
    let target = signal_target(&shared, thread);
    let (_, target) = match target {
        Ok(target) => target,
        Err(code) => return code,
    };
    // SAFETY: the calling thread's own, live `JvmThread`, which this call
    // runs on.
    crate::debug::inspect::jvmti_interrupt(&shared, unsafe { &mut *current }, target);
    ERR_NONE
}

/// Slot 7: `StopThread(env, thread, exception)` (`can_signal_thread`): the
/// thread throws `exception` at its next bytecode, and at once from a
/// `sleep`, `wait` or `park`, as `ThreadReference.Stop` makes it
/// (`debug::inspect::jvmti_stop`; MEASURED: a sleeping thread catches the
/// agent's `IllegalStateException`). A NULL thread is `INVALID_THREAD`
/// (MEASURED), a NULL exception `NULL_POINTER`. A virtual thread must be the
/// calling one or suspended (`THREAD_NOT_SUSPENDED`), as JDK 21 requires.
pub(super) extern "C" fn stop_thread(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    exception: JObject,
) -> JInt {
    let shared = match live_with_cap(env, CAP_SIGNAL_THREAD) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if thread == 0 {
        return ERR_INVALID_THREAD;
    }
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    let _fx = jni::ForeignJniEntry::enter();
    let (tid, target) = match signal_target(&shared, thread) {
        Ok(target) => target,
        Err(code) => return code,
    };
    if exception == 0 {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
    let current_tid = unsafe { (*current).thread_id.0 };
    if shared.threads.virtual_thread_manager.is_virtual(tid)
        && tid != current_tid
        && !shared.debug.debug_state.lock().is_thread_suspended(tid)
    {
        return ERR_THREAD_NOT_SUSPENDED;
    }
    let prev = jni::replace_jni_context(&shared);
    let throwable = jni::jobject_to_obj(exception);
    jni::restore_jni_context(prev);
    let Some(throwable) = throwable else {
        return ERR_INVALID_OBJECT;
    };
    // SAFETY: as in [`interrupt_thread`].
    match crate::debug::inspect::jvmti_stop(
        &shared,
        unsafe { &mut *current },
        tid,
        target,
        throwable,
    ) {
        Ok(()) => ERR_NONE,
        Err(_) => ERR_INVALID_OBJECT,
    }
}

// --- RunAgentThread -----------------------------------------------------------

/// `void (JNICALL *jvmtiStartFunction)(jvmtiEnv*, JNIEnv*, void* arg)`.
type StartFunction = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, usize);

/// What a `RunAgentThread` thread runs in place of its `Thread.run()`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AgentThreadBody {
    env: usize,
    proc: usize,
    arg: usize,
}

thread_local! {
    /// The body `RunAgentThread` hands `thread_start` on the calling thread,
    /// for the one start it makes ([`take_pending_agent_body`]).
    static PENDING_AGENT_BODY: std::cell::Cell<Option<AgentThreadBody>> = const { std::cell::Cell::new(None) };
}

/// The agent body the calling thread's `RunAgentThread` is starting a thread
/// for, taken by `vm_exec`'s `thread_start` before it spawns the thread.
/// `None` -- one TLS read -- for every ordinary `Thread.start`.
pub(crate) fn take_pending_agent_body() -> Option<AgentThreadBody> {
    PENDING_AGENT_BODY.with(std::cell::Cell::take)
}

/// Run an agent thread's body on the new thread `thread` (in place of
/// `run()`): the JNI context and the thread's own `JvmThread` installed and a
/// local frame pushed, as around a native call, then
/// `proc(jvmti_env, jni_env, arg)`.
pub(crate) fn run_agent_thread_body(
    shared: &SharedVm,
    thread: &mut JvmThread,
    body: AgentThreadBody,
) {
    let depth = jni::local_frame_depth();
    jni::push_local_frame(16);
    let prev_vm = jni::replace_jni_context(shared);
    // SAFETY: this thread's own `JvmThread`, which it runs.
    let prev_thread = unsafe { jni::replace_jni_thread(thread as *mut JvmThread) };
    let raw_jni_locals = jni::RawLocalsDispatch::enter();
    // gci i15/hx (`gci-i15hx-jvmti-tag-agent-hangs-with-native-transitions-FIXED-20261014`):
    // the agent's C code runs IN NATIVE, as a JNI native method's does
    // (`CRATONVM_JNI_NATIVE_TRANSITIONS`, default ON): excluded from every
    // pause, and each JNIEnv / C JVMTI function it calls leaves native for its
    // body (`ForeignJniEntry::enter_from_native`). HotSpot runs a
    // `RunAgentThread` start function in `_thread_in_native` too. It used to
    // run as a COUNTED mutator that never reaches a poll: every pause
    // requested while the agent ran waited for `proc` to return -- for good,
    // when the agent's C code waits on something a parked Java thread does.
    // Inert (the old counted run) with the package off or raw locals here
    // (`JniNativeCall::enter`'s rules). Dropped before the context below is
    // restored: the leave needs this thread's `JvmThread` installed.
    let in_native = jni::JniNativeCall::enter(jni::raw_local_escapes());
    // SAFETY: the agent's `jvmtiStartFunction`, checked non-null by
    // `RunAgentThread`.
    unsafe {
        let proc: StartFunction = std::mem::transmute::<usize, StartFunction>(body.proc);
        proc(
            body.env as *mut JvmtiNativeEnv,
            jni::get_jni_env(),
            body.arg,
        );
    }
    drop(in_native);
    drop(raw_jni_locals);
    // SAFETY: the pointer this thread had installed before.
    unsafe { jni::restore_jni_thread(prev_thread) };
    jni::restore_jni_context(prev_vm);
    jni::truncate_local_frames(depth);
}

/// Slot 12: `RunAgentThread(env, thread, proc, arg, priority)` (live phase):
/// `thread`, a `java.lang.Thread` that is not running, is started as a
/// daemon thread of `priority` that runs `proc(env, jni_env, arg)` instead of
/// its `run()`, and ends when `proc` returns. HotSpot's checks in its order
/// (MEASURED): a NULL `proc` 100; a NULL thread, an object that is no
/// `Thread`, or a running thread `INVALID_THREAD`; a priority outside 1..10
/// `INVALID_PRIORITY` (12). A `Thread` that ran and terminated is started
/// again (MEASURED: 0).
pub(super) extern "C" fn run_agent_thread(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    proc: usize,
    arg: usize,
    priority: JInt,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if proc == 0 {
        return ERR_NULL_POINTER;
    }
    if thread == 0 {
        return ERR_INVALID_THREAD;
    }
    match lookup_thread(&shared, thread) {
        Ok(ThreadLookup::Live(_)) => return ERR_INVALID_THREAD,
        Ok(ThreadLookup::Dead | ThreadLookup::Unstarted) => {}
        Err(code) => return code,
    }
    if !(1..=10).contains(&priority) {
        return ERR_INVALID_PRIORITY;
    }
    let Some(current) = jni::current_jvm_thread_of(&shared) else {
        return ERR_UNATTACHED_THREAD;
    };
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(&shared);
    let obj = jni::jobject_to_obj(thread);
    jni::restore_jni_context(prev);
    let Some(obj) = obj else {
        return ERR_INVALID_THREAD;
    };
    let mut ctx = crate::vm::vm_exec::NativeContextImpl {
        shared: &shared,
        // SAFETY: the calling thread's own, live `JvmThread`, which this
        // call runs on.
        thread: unsafe { &mut *current },
    };
    ctx.prepare_agent_thread(obj, priority);
    PENDING_AGENT_BODY.with(|cell| {
        cell.set(Some(AgentThreadBody {
            env: env as usize, // Cast: the env's address, handed back to `proc`
            proc,
            arg,
        }))
    });
    let started = {
        use cratonvm_native_api::NativeThreadAccess as _;
        ctx.thread_start(obj)
    };
    // A start that failed before it took the body leaves none behind.
    let _ = take_pending_agent_body();
    match started {
        Ok(_) => ERR_NONE,
        Err(_) => ERR_INTERNAL,
    }
}

// --- Suspend/ResumeAllVirtualThreads -------------------------------------------

/// The live virtual threads of `shared` but the ones `except` names.
fn virtual_threads_except(shared: &SharedVm, except: &[JObject]) -> Result<Vec<u64>, JInt> {
    let mut excluded = Vec::new();
    for &handle in except {
        match lookup_thread(shared, handle)? {
            ThreadLookup::Live(tid) => excluded.push(tid),
            ThreadLookup::Dead | ThreadLookup::Unstarted => {}
        }
    }
    let registry = &shared.threads.thread_registry;
    Ok(registry
        .all_thread_names()
        .into_iter()
        .map(|(tid, _)| tid.0)
        .filter(|&tid| {
            registry.is_alive(ThreadId(tid))
                && shared.threads.virtual_thread_manager.is_virtual(tid)
                && !excluded.contains(&tid)
        })
        .collect())
}

/// The checks of slots 118 / 119 (MEASURED): `can_support_virtual_threads`
/// and `can_suspend` (99), a negative count `ILLEGAL_ARGUMENT`, a NULL list
/// for a positive count `NULL_POINTER`. Answers the VM and the excepted
/// handles.
fn all_virtual_args(
    env: *mut JvmtiNativeEnv,
    except_count: JInt,
    except_list: *const JObject,
) -> Result<(Arc<SharedVm>, Vec<JObject>), JInt> {
    let shared = live_with_cap(env, CAP_SUPPORT_VIRTUAL_THREADS)?;
    if !live_env(env)?.possesses(CAN_SUSPEND) {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    let count = usize::try_from(except_count).map_err(|_| ERR_ILLEGAL_ARGUMENT)?;
    if count > 0 && except_list.is_null() {
        return Err(ERR_NULL_POINTER);
    }
    let handles = if count == 0 {
        Vec::new()
    } else {
        // SAFETY: the agent's list of `except_count` handles, non-null.
        unsafe { std::slice::from_raw_parts(except_list, count) }.to_vec()
    };
    Ok((shared, handles))
}

/// Slot 118: `SuspendAllVirtualThreads(env, except_count, except_list)`:
/// every live virtual thread but the listed ones and the ones already
/// suspended, each as `SuspendThread` suspends it (MEASURED: the listed
/// one stays running).
pub(super) extern "C" fn suspend_all_virtual_threads(
    env: *mut JvmtiNativeEnv,
    except_count: JInt,
    except_list: *const JObject,
) -> JInt {
    let (shared, except) = match all_virtual_args(env, except_count, except_list) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let targets = match virtual_threads_except(&shared, &except) {
        Ok(targets) => targets,
        Err(code) => return code,
    };
    let current = jni::current_jvm_thread_of(&shared)
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        .map(|current| unsafe { (*current).thread_id.0 });
    let mut marked = Vec::new();
    for tid in targets {
        if mark_suspended(&shared, tid) == ERR_NONE {
            marked.push(tid);
        }
    }
    for &tid in marked.iter().filter(|&&tid| Some(tid) != current) {
        await_suspended(&shared, tid, false);
    }
    if let Some(tid) = current.filter(|tid| marked.contains(tid)) {
        await_suspended(&shared, tid, true);
    }
    ERR_NONE
}

/// Slot 119: `ResumeAllVirtualThreads(env, except_count, except_list)`:
/// every suspended virtual thread but the listed ones resumed.
pub(super) extern "C" fn resume_all_virtual_threads(
    env: *mut JvmtiNativeEnv,
    except_count: JInt,
    except_list: *const JObject,
) -> JInt {
    let (shared, except) = match all_virtual_args(env, except_count, except_list) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let targets = match virtual_threads_except(&shared, &except) {
        Ok(targets) => targets,
        Err(code) => return code,
    };
    for tid in targets {
        let _ = mark_resumed(&shared, tid);
    }
    ERR_NONE
}

// --- SetHeapSamplingInterval -------------------------------------------------

/// Slot 156: `SetHeapSamplingInterval(env, sampling_interval)`
/// (`can_generate_sampled_object_alloc_events`; OnLoad or live phase): the
/// mean bytes between `SampledObjectAlloc` samples, 0 for every allocation;
/// negative is `ILLEGAL_ARGUMENT` (MEASURED).
pub(super) extern "C" fn set_heap_sampling_interval(
    env: *mut JvmtiNativeEnv,
    interval: JInt,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    match live_env(env) {
        Ok(e) if !env_has(e, CAP_SAMPLED_OBJECT_ALLOC_EVENTS) => {
            return ERR_MUST_POSSESS_CAPABILITY
        }
        Ok(_) => {}
        Err(code) => return code,
    }
    if interval < 0 {
        return ERR_ILLEGAL_ARGUMENT;
    }
    match phase {
        PropertyPhase::OnLoad => STARTUP_ENVS.with(|c| {
            if let Some(scope) = c.borrow_mut().as_mut() {
                scope.effects.heap_sampling_interval = Some(interval);
            }
        }),
        PropertyPhase::Live(shared) => {
            shared.debug.jvmti_env.lock().heap_sampling_interval = interval;
            super::events_more::reset_sampling(&shared);
        }
    }
    ERR_NONE
}

// --- SetNativeMethodPrefix(es) -------------------------------------------------

/// The capability check and phase of slots 73 / 74.
fn prefix_env<'a>(env: *mut JvmtiNativeEnv) -> Result<&'a JvmtiNativeEnv, JInt> {
    property_phase(env)?;
    let e = live_env(env)?;
    if !env_has(e, CAP_SET_NATIVE_METHOD_PREFIX) {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    Ok(e)
}

/// Slot 73: `SetNativeMethodPrefix(env, prefix)`: this env's one prefix;
/// NULL removes them (MEASURED: 0).
pub(super) extern "C" fn set_native_method_prefix(
    env: *mut JvmtiNativeEnv,
    prefix: *const std::ffi::c_char,
) -> JInt {
    let e = match prefix_env(env) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let prefixes = if prefix.is_null() {
        Vec::new()
    } else {
        // SAFETY: the agent's NUL-terminated string.
        vec![unsafe { agent_c_string(prefix) }]
    };
    e.state.lock().native_prefixes = prefixes;
    ERR_NONE
}

/// Slot 74: `SetNativeMethodPrefixes(env, prefix_count, prefixes)`: this
/// env's prefixes, in order; a negative count `ILLEGAL_ARGUMENT`, a NULL
/// array for a positive count `NULL_POINTER`.
pub(super) extern "C" fn set_native_method_prefixes(
    env: *mut JvmtiNativeEnv,
    count: JInt,
    prefixes: *const *const std::ffi::c_char,
) -> JInt {
    let e = match prefix_env(env) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let Ok(count) = usize::try_from(count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if count > 0 && prefixes.is_null() {
        return ERR_NULL_POINTER;
    }
    let mut list = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: the agent's array of `count` strings.
        let p = unsafe { *prefixes.add(i) };
        if p.is_null() {
            return ERR_NULL_POINTER;
        }
        // SAFETY: a NUL-terminated string of the agent's.
        list.push(unsafe { agent_c_string(p) });
    }
    e.state.lock().native_prefixes = list;
    ERR_NONE
}

/// Every native method prefix the C envs of `shared` set, env by env in the
/// order they were bound, each env's in its order, as HotSpot's
/// `JvmtiExport::get_all_native_method_prefixes` lists them; the native
/// linker strips them last first (`vm_exec::prefixed_native_link`).
pub(crate) fn all_native_method_prefixes(shared: &SharedVm) -> Vec<String> {
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    let mut out = Vec::new();
    for addr in envs {
        // SAFETY: an address `new_env_for_version` handed out.
        let state = unsafe { env_at(addr) }.state.lock();
        if !state.disposed {
            out.extend(
                state
                    .native_prefixes
                    .iter()
                    .filter(|p| !p.is_empty())
                    .cloned(),
            );
        }
    }
    out
}

#[cfg(test)]
mod gci_i15hx_agent_thread_tests {
    /// gci i15/hx (`gci-i15hx-jvmti-tag-agent-hangs-with-native-transitions-FIXED-20261014`):
    /// a `RunAgentThread` start function runs in native -- the in-native
    /// bracket opens after this thread's JNI context, `JvmThread` and local
    /// frame are installed, around exactly the `proc` call, and closes before
    /// the context is restored (its leave needs the `JvmThread`).
    #[test]
    fn gci_i15hx_the_agent_start_function_runs_in_native() {
        let src = include_str!("agent_functions.rs").replace("\r\n", "\n");
        let start = src
            .find("pub(crate) fn run_agent_thread_body(")
            .expect("run_agent_thread_body");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("the function's end")];
        let pos = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
        let enter = pos("jni::JniNativeCall::enter(jni::raw_local_escapes())");
        assert!(pos("jni::push_local_frame(") < enter);
        assert!(pos("jni::replace_jni_context(shared)") < enter);
        assert!(pos("jni::replace_jni_thread(") < enter);
        assert!(pos("jni::RawLocalsDispatch::enter()") < enter);
        // gci i21/fx: rustfmt lays the call out one argument per line, so the
        // call is the transmuted `proc` followed by its first argument.
        let call = pos("std::mem::transmute::<usize, StartFunction>(body.proc)");
        assert!(call < pos("body.env as *mut JvmtiNativeEnv"));
        let leave = pos("drop(in_native);");
        assert!(enter < call && call < leave);
        assert!(leave < pos("drop(raw_jni_locals);"));
        assert!(leave < pos("jni::restore_jni_thread(prev_thread)"));
        assert!(leave < pos("jni::restore_jni_context(prev_vm)"));
    }
}
