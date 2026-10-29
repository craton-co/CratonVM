// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The extension mechanism of the C table (interpreter round i2 wave 63,
//! lane L1; the i51-L1 page's "Next step (from wave 61)", item 2): slots 124
//! `GetExtensionFunctions`, 125 `GetExtensionEvents` and 126
//! `SetExtensionEventCallback`, and HotSpot 25's three extension functions
//! and three extension events, with HotSpot's ids, descriptions, parameters
//! and errors (MEASURED, `tools/probes/interp/L1/L1W63JvmtiAgentFunctions.java`'s
//! `ext` rows). An agent that looks up `GetVirtualThread` and gets 98 may
//! stop.

use super::*;

// `jvmtiParamKind` / `jvmtiParamTypes` (`jvmti.h`).
const KIND_IN: JInt = 91;
const KIND_IN_PTR: JInt = 92;
const KIND_OUT: JInt = 96;
const TYPE_JBOOLEAN: JInt = 108;
const TYPE_JTHREAD: JInt = 110;
const TYPE_CCHAR: JInt = 115;
const TYPE_JNIENV: JInt = 117;

/// HotSpot's extension events (`EXT_EVENT_*`), below [`MIN_EVENT`].
pub(super) const EXT_EVENT_CLASS_UNLOAD: JInt = 49;
pub(super) const EXT_EVENT_VIRTUAL_THREAD_MOUNT: JInt = 48;
pub(super) const EXT_EVENT_VIRTUAL_THREAD_UNMOUNT: JInt = 47;

/// `jvmtiParamInfo`.
#[repr(C)]
struct ParamInfo {
    name: *mut u8,
    kind: JInt,
    base_type: JInt,
    null_ok: u8,
}

/// `jvmtiExtensionFunctionInfo`.
#[repr(C)]
struct ExtensionFunctionInfo {
    func: usize,
    id: *mut u8,
    short_description: *mut u8,
    param_count: JInt,
    params: *mut ParamInfo,
    error_count: JInt,
    errors: *mut JInt,
}

/// `jvmtiExtensionEventInfo`.
#[repr(C)]
struct ExtensionEventInfo {
    extension_event_index: JInt,
    id: *mut u8,
    short_description: *mut u8,
    param_count: JInt,
    params: *mut ParamInfo,
}

type Param = (&'static str, JInt, JInt);

/// HotSpot's extension functions, in its order:
/// `(function, id, description, params, errors)`.
fn functions() -> [(usize, &'static str, &'static str, Vec<Param>, Vec<JInt>); 3] {
    [
        (
            is_class_unloading_enabled as *const () as usize,
            "com.sun.hotspot.functions.IsClassUnloadingEnabled",
            "Tell if class unloading is enabled (-noclassgc)",
            vec![("IsClassUnloadingEnabled", KIND_OUT, TYPE_JBOOLEAN)],
            Vec::new(),
        ),
        (
            get_virtual_thread as *const () as usize,
            "com.sun.hotspot.functions.GetVirtualThread",
            "Get virtual thread executed on carrier thread",
            vec![
                ("GetVirtualThread", KIND_IN, TYPE_JTHREAD),
                ("GetVirtualThread", KIND_OUT, TYPE_JTHREAD),
            ],
            vec![ERR_MUST_POSSESS_CAPABILITY, ERR_INVALID_THREAD],
        ),
        (
            get_carrier_thread as *const () as usize,
            "com.sun.hotspot.functions.GetCarrierThread",
            "Get carrier thread executing virtual thread",
            vec![
                ("GetCarrierThread", KIND_IN, TYPE_JTHREAD),
                ("GetCarrierThread", KIND_OUT, TYPE_JTHREAD),
            ],
            vec![ERR_MUST_POSSESS_CAPABILITY, ERR_INVALID_THREAD],
        ),
    ]
}

/// HotSpot's extension events, in its order: `(index, id, description,
/// params)`.
fn events() -> [(JInt, &'static str, &'static str, Vec<Param>); 3] {
    [
        (
            EXT_EVENT_CLASS_UNLOAD,
            "com.sun.hotspot.events.ClassUnload",
            "CLASS_UNLOAD event",
            vec![
                ("JNI Environment", KIND_IN_PTR, TYPE_JNIENV),
                ("Class", KIND_IN_PTR, TYPE_CCHAR),
            ],
        ),
        (
            EXT_EVENT_VIRTUAL_THREAD_MOUNT,
            "com.sun.hotspot.events.VirtualThreadMount",
            "VIRTUAL_THREAD_MOUNT event",
            vec![
                ("JNI Environment", KIND_IN_PTR, TYPE_JNIENV),
                ("Virtual Thread", KIND_IN, TYPE_JTHREAD),
            ],
        ),
        (
            EXT_EVENT_VIRTUAL_THREAD_UNMOUNT,
            "com.sun.hotspot.events.VirtualThreadUnmount",
            "VIRTUAL_THREAD_UNMOUNT event",
            vec![
                ("JNI Environment", KIND_IN_PTR, TYPE_JNIENV),
                ("Virtual Thread", KIND_IN, TYPE_JTHREAD),
            ],
        ),
    ]
}

/// `params` as a fresh `Allocate` block of `jvmtiParamInfo`, each name its
/// own block, as HotSpot hands them out.
fn allocate_params(params: &[Param]) -> Result<*mut ParamInfo, JInt> {
    let block =
        allocate_block(params.len().max(1) * std::mem::size_of::<ParamInfo>()).cast::<ParamInfo>();
    if block.is_null() {
        return Err(ERR_OUT_OF_MEMORY);
    }
    for (i, &(name, kind, base_type)) in params.iter().enumerate() {
        let name = allocate_c_string(name);
        if name.is_null() {
            return Err(ERR_OUT_OF_MEMORY);
        }
        // SAFETY: `block` holds `params.len()` entries.
        unsafe {
            block.add(i).write(ParamInfo {
                name,
                kind,
                base_type,
                null_ok: 0,
            });
        }
    }
    Ok(block)
}

/// Slot 124: `GetExtensionFunctions(env, extension_count_ptr,
/// extensions)`. Any phase.
pub(super) extern "C" fn get_extension_functions(
    env: *mut JvmtiNativeEnv,
    count_ptr: *mut JInt,
    extensions_ptr: *mut *mut u8,
) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if count_ptr.is_null() || extensions_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let table = functions();
    let block = allocate_block(table.len() * std::mem::size_of::<ExtensionFunctionInfo>())
        .cast::<ExtensionFunctionInfo>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    for (i, (func, id, description, params, errors)) in table.iter().enumerate() {
        let params_block = match allocate_params(params) {
            Ok(block) => block,
            Err(code) => return code,
        };
        let errors_block =
            allocate_block(errors.len().max(1) * std::mem::size_of::<JInt>()).cast::<JInt>();
        if errors_block.is_null() {
            return ERR_OUT_OF_MEMORY;
        }
        // SAFETY: fresh blocks of the sizes allocated above.
        unsafe {
            std::ptr::copy_nonoverlapping(errors.as_ptr(), errors_block, errors.len());
            block.add(i).write(ExtensionFunctionInfo {
                func: *func,
                id: allocate_c_string(id),
                short_description: allocate_c_string(description),
                param_count: params.len() as JInt, // Cast: at most two
                params: params_block,
                error_count: errors.len() as JInt, // Cast: at most two
                errors: errors_block,
            });
        }
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = table.len() as JInt; // Cast: three
        *extensions_ptr = block.cast();
    }
    ERR_NONE
}

/// Slot 125: `GetExtensionEvents(env, extension_count_ptr, extensions)`.
/// Any phase.
pub(super) extern "C" fn get_extension_events(
    env: *mut JvmtiNativeEnv,
    count_ptr: *mut JInt,
    extensions_ptr: *mut *mut u8,
) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if count_ptr.is_null() || extensions_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let table = events();
    let block = allocate_block(table.len() * std::mem::size_of::<ExtensionEventInfo>())
        .cast::<ExtensionEventInfo>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    for (i, (index, id, description, params)) in table.iter().enumerate() {
        let params_block = match allocate_params(params) {
            Ok(block) => block,
            Err(code) => return code,
        };
        // SAFETY: a fresh block of `table.len()` entries.
        unsafe {
            block.add(i).write(ExtensionEventInfo {
                extension_event_index: *index,
                id: allocate_c_string(id),
                short_description: allocate_c_string(description),
                param_count: params.len() as JInt, // Cast: two
                params: params_block,
            });
        }
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = table.len() as JInt; // Cast: three
        *extensions_ptr = block.cast();
    }
    ERR_NONE
}

/// Slot 126: `SetExtensionEventCallback(env, extension_event_index,
/// callback)`: the callback for one of [`events`] (NULL clears it); another
/// index is `JVMTI_ERROR_ILLEGAL_ARGUMENT` (MEASURED: 46 and 50). As on
/// HotSpot an extension event reaches the agent while both its callback is
/// set and `SetEventNotificationMode` enabled it.
pub(super) extern "C" fn set_extension_event_callback(
    env: *mut JvmtiNativeEnv,
    index: JInt,
    callback: usize,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    if !(EXT_EVENT_VIRTUAL_THREAD_UNMOUNT..=EXT_EVENT_CLASS_UNLOAD).contains(&index) {
        return ERR_ILLEGAL_ARGUMENT;
    }
    let slot = (index - EXT_EVENT_VIRTUAL_THREAD_UNMOUNT) as usize; // Cast: 0..=2
    env.state.lock().ext_callbacks[slot] = callback;
    if let Some(shared) = env.vm() {
        super::events_more::refresh_event_gates(&shared);
    }
    ERR_NONE
}

/// The extension callback of `index` for the env at `env_addr`, when it is
/// live and has the event enabled for thread `tid` (0: a global event).
pub(super) fn ext_callback_for(env_addr: usize, index: JInt, tid: u64) -> Option<usize> {
    // SAFETY: an address `new_env_for_version` handed out.
    let env = unsafe { env_at(env_addr) };
    let state = env.state.lock();
    if state.disposed || !state.enabled_for(index, tid) {
        return None;
    }
    let slot = usize::try_from(index - EXT_EVENT_VIRTUAL_THREAD_UNMOUNT).ok()?;
    state.ext_callbacks.get(slot).copied().filter(|&f| f != 0)
}

/// `com.sun.hotspot.functions.IsClassUnloadingEnabled(env, jboolean*)`:
/// HotSpot's `ClassUnloading` flag. zt t21/tg10: this VM's own
/// (`LoaderIdentityTables::class_unloading`, from `VmConfig::class_unloading`):
/// `JNI_FALSE` under `-XX:-ClassUnloading` / `-Xnoclassgc`, `JNI_TRUE` by
/// default (the answer before, whatever the command line said).
///
/// zt t23/x19 (review page item 28e; `CRATONVM_JVMTI_ONLOAD_CLASS_UNLOADING`,
/// default ON, `=0` kill): HotSpot reads the already-parsed flag in ANY phase,
/// `Agent_OnLoad` included, where a startup agent's env has no VM yet; that
/// env answers the bit its VM noted before loading the agents
/// (`note_onload_class_unloading`). It answered `JNI_TRUE` there under
/// `-Xnoclassgc`. `=0`: `JNI_TRUE` without a VM, as before.
extern "C" fn is_class_unloading_enabled(env: *mut JvmtiNativeEnv, enabled: *mut u8) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let on = match env.vm() {
        Some(shared) => shared.loader_identity.class_unloading(),
        None => onload_class_unloading_answer(env.onload_class_unloading(), || {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JVMTI_ONLOAD_CLASS_UNLOADING")
        }),
    };
    write_jboolean(enabled, Ok(on))
}

/// zt t23/x19: [`is_class_unloading_enabled`]'s answer for an env without a
/// VM: the bit its OnLoad scope noted, unless the switch (`enabled`, read
/// only when a bit was noted) is killed; `true` otherwise. Pure but for the
/// switch read.
fn onload_class_unloading_answer(noted: Option<bool>, enabled: impl FnOnce() -> bool) -> bool {
    match noted {
        Some(bit) if enabled() => bit,
        _ => true,
    }
}

/// The thread a `jthread` (NULL: the calling thread) names, as the two
/// thread extension functions read it: `(object, live VM thread id)`.
fn thread_operand(shared: &SharedVm, thread: JObject) -> Result<(ObjectRef, Option<u64>), JInt> {
    if thread == 0 {
        let current = jni::current_jvm_thread_of(shared).ok_or(ERR_UNATTACHED_THREAD)?;
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        let tid = unsafe { (*current).thread_id };
        let obj = shared
            .threads
            .thread_registry
            .java_thread_obj(tid)
            .ok_or(ERR_INVALID_THREAD)?;
        return Ok((obj, Some(tid.0)));
    }
    let live = match lookup_thread(shared, thread)? {
        ThreadLookup::Live(tid) => Some(tid),
        ThreadLookup::Dead | ThreadLookup::Unstarted => None,
    };
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(thread);
    jni::restore_jni_context(prev);
    Ok((obj.ok_or(ERR_INVALID_THREAD)?, live))
}

/// Is `obj` a `java.lang.VirtualThread` (a `BaseVirtualThread`)?
pub(super) fn is_virtual_thread_object(shared: &SharedVm, obj: ObjectRef) -> bool {
    let class_id = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .read()
        .is_subclass_of_by_name(class_id, "java/lang/BaseVirtualThread")
}

/// A local reference to `obj` in `shared`'s JNI context.
fn local_of(shared: &SharedVm, obj: Option<ObjectRef>) -> JObject {
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let handle = obj.map_or(0, jni::new_local_handle);
    jni::restore_jni_context(prev);
    handle
}

/// `com.sun.hotspot.functions.GetVirtualThread(env, jthread carrier,
/// jthread* vthread)`: the virtual thread mounted on platform thread
/// `carrier`, NULL when none is. This VM runs a mounted virtual thread on a
/// pool OS thread that has no `java.lang.Thread` of its own; the carrier
/// objects [`get_carrier_thread`] hands out stand for those OS threads, so
/// the virtual thread mounted on one is the virtual thread whose OS thread
/// it names. Any other platform thread carries none. HotSpot's checks in its
/// order (MEASURED): the capability (99), the thread (10; a virtual thread
/// too), the out-parameter (100).
extern "C" fn get_virtual_thread(
    env: *mut JvmtiNativeEnv,
    carrier: JObject,
    vthread_ptr: *mut JObject,
) -> JInt {
    let shared = match live_phase_vm_with(
        env,
        CAP_SUPPORT_VIRTUAL_THREADS / 32,
        1 << (CAP_SUPPORT_VIRTUAL_THREADS % 32),
    ) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
    // finding 3): ONE entry for the whole function. `thread_operand` returns a
    // raw `ObjectRef` (and, for NULL, reads the registry's mirror outside any
    // entry); a caller in native -- an agent thread since gci i15/hx, a JNI
    // native's C code -- then read its class, fields and global-ref identity
    // back in native, where any pause may move it. Nested entries are inert.
    let _fx = jni::ForeignJniEntry::enter();
    let (obj, _live) = match thread_operand(&shared, carrier) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if vthread_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    if is_virtual_thread_object(&shared, obj) {
        return ERR_INVALID_THREAD;
    }
    // The JDK drives virtual threads (`vm::jdk_continuation`): a carrier is
    // a real platform thread, hosting the continuation it runs.
    let vtm = &shared.threads.virtual_thread_manager;
    let mounted = if vtm.jdk_continuations() {
        crate::vm::vm_exec::resolve_thread_id_from_thread_obj(&shared, obj)
            .and_then(|carrier| vtm.continuation_runners().hosted_by(carrier))
            .map(|tid| tid.0)
    } else {
        carrier_os_tid(&shared, obj).and_then(|os_tid| mounted_virtual_on(&shared, os_tid))
    };
    let vthread = mounted.and_then(|tid| {
        shared
            .threads
            .thread_registry
            .java_thread_obj(ThreadId(tid))
    });
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *vthread_ptr = local_of(&shared, vthread) };
    ERR_NONE
}

/// `com.sun.hotspot.functions.GetCarrierThread(env, jthread vthread,
/// jthread* carrier)`: the platform thread a mounted virtual thread runs on,
/// NULL for an unmounted one (MEASURED: a parked virtual thread answers
/// NULL; a platform thread, NULL included when the caller is one, 10).
extern "C" fn get_carrier_thread(
    env: *mut JvmtiNativeEnv,
    vthread: JObject,
    carrier_ptr: *mut JObject,
) -> JInt {
    let shared = match live_phase_vm_with(
        env,
        CAP_SUPPORT_VIRTUAL_THREADS / 32,
        1 << (CAP_SUPPORT_VIRTUAL_THREADS % 32),
    ) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // gci i15/rb finding 3: one entry for the whole function, as in
    // `get_virtual_thread`.
    let _fx = jni::ForeignJniEntry::enter();
    let (obj, live) = match thread_operand(&shared, vthread) {
        Ok(found) => found,
        Err(code) => return code,
    };
    if !is_virtual_thread_object(&shared, obj) {
        return ERR_INVALID_THREAD;
    }
    if carrier_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let registry = &shared.threads.thread_registry;
    let vtm = &shared.threads.virtual_thread_manager;
    let handle = if vtm.jdk_continuations() {
        // The real carrier (`vm::jdk_continuation`): the platform thread
        // whose `Continuation.enterSpecial` runs it now.
        let carrier = live
            .and_then(|tid| vtm.continuation_runners().carrier_of(ThreadId(tid)))
            .and_then(|carrier| registry.java_thread_obj(carrier));
        local_of(&shared, carrier)
    } else {
        let os_tid = live
            .filter(|&tid| registry.is_mounted(ThreadId(tid)))
            .and_then(|tid| registry.os_tid_of(ThreadId(tid)));
        match os_tid {
            Some(os_tid) => carrier_object(&shared, os_tid),
            None => 0,
        }
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *carrier_ptr = handle };
    ERR_NONE
}

/// The live virtual thread mounted on OS thread `os_tid`, if any.
fn mounted_virtual_on(shared: &SharedVm, os_tid: u32) -> Option<u64> {
    let registry = &shared.threads.thread_registry;
    registry
        .all_thread_names()
        .into_iter()
        .map(|(tid, _)| tid)
        .find(|&tid| {
            registry.is_alive(tid)
                && registry.is_mounted(tid)
                && registry.os_tid_of(tid) == Some(os_tid)
                && registry
                    .java_thread_obj(tid)
                    .is_some_and(|obj| is_virtual_thread_object(shared, obj))
        })
        .map(|tid| tid.0)
}

/// The OS thread a carrier object [`carrier_object`] made stands for.
fn carrier_os_tid(shared: &SharedVm, obj: ObjectRef) -> Option<u32> {
    let carriers = shared.debug.jvmti_env.lock().carrier_threads.clone();
    let refs = shared.natives.jni_global_refs.lock();
    carriers
        .iter()
        .find(|&&(_, global)| refs.resolve(global) == Some(obj))
        .map(|&(os_tid, _)| os_tid)
}

/// A local reference to the `java.lang.Thread` standing for carrier OS
/// thread `os_tid`, made the first time one is asked for: an unstarted
/// platform thread named as HotSpot names its carriers
/// (`ForkJoinPool-1-worker-<n>`, n counting the carriers asked about), kept
/// by a JNI global reference for the VM's life. NULL when it cannot be made.
fn carrier_object(shared: &SharedVm, os_tid: u32) -> JObject {
    let known = shared
        .debug
        .jvmti_env
        .lock()
        .carrier_threads
        .iter()
        .find(|&&(tid, _)| tid == os_tid)
        .map(|&(_, global)| global);
    let global = match known {
        Some(global) => global,
        None => {
            let ordinal = shared.debug.jvmti_env.lock().carrier_threads.len() + 1;
            let Some(global) =
                new_platform_thread(shared, &format!("ForkJoinPool-1-worker-{ordinal}"))
            else {
                return 0;
            };
            shared
                .debug
                .jvmti_env
                .lock()
                .carrier_threads
                .push((os_tid, global));
            global
        }
    };
    // Not `global_to_local`, which deletes the global: the carrier's global
    // reference must outlive this call (`carrier_os_tid` resolves it).
    let obj = shared.natives.jni_global_refs.lock().resolve(global);
    local_of(shared, obj)
}

/// A global reference to a new, unstarted `java.lang.Thread(name)`, built
/// through JNI on the calling (agent) thread.
fn new_platform_thread(shared: &SharedVm, name: &str) -> Option<JObject> {
    type FindClass = unsafe extern "C" fn(JNIEnv, *const std::ffi::c_char) -> JObject;
    type GetMethodId = unsafe extern "C" fn(
        JNIEnv,
        JObject,
        *const std::ffi::c_char,
        *const std::ffi::c_char,
    ) -> u64;
    type NewObjectA = unsafe extern "C" fn(JNIEnv, JObject, u64, *const u64) -> JObject;
    type NewStringUtf = unsafe extern "C" fn(JNIEnv, *const std::ffi::c_char) -> JObject;
    type NewGlobalRef = unsafe extern "C" fn(JNIEnv, JObject) -> JObject;
    type ExceptionClear = unsafe extern "C" fn(JNIEnv);
    let prev = jni::replace_jni_context(shared);
    let env = jni::get_jni_env();
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `env` is this VM's `JNIEnv*`; each slot is the `jni.h`
    // function of that index.
    let made = unsafe {
        let slot = |n: usize| *(*env).add(n);
        let find_class: FindClass = std::mem::transmute(slot(6));
        let get_method_id: GetMethodId = std::mem::transmute(slot(33));
        let new_object: NewObjectA = std::mem::transmute(slot(30));
        let new_string: NewStringUtf = std::mem::transmute(slot(167));
        let new_global: NewGlobalRef = std::mem::transmute(slot(21));
        let clear: ExceptionClear = std::mem::transmute(slot(17));
        let class = find_class(env, c"java/lang/Thread".as_ptr());
        let init = if class == 0 {
            0
        } else {
            get_method_id(
                env,
                class,
                c"<init>".as_ptr(),
                c"(Ljava/lang/String;)V".as_ptr(),
            )
        };
        let text = new_string(env, name.as_ptr());
        let thread = if init == 0 || text == 0 {
            0
        } else {
            let args = [text as u64]; // Cast: a `jvalue` holding the `jobject`
            new_object(env, class, init, args.as_ptr())
        };
        clear(env);
        if thread == 0 {
            None
        } else {
            Some(new_global(env, thread))
        }
    };
    jni::restore_jni_context(prev);
    made.filter(|&g| g != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HotSpot 25's extension list, ids and order (MEASURED,
    /// `L1W63JvmtiAgentFunctions`).
    #[test]
    fn the_extensions_are_hotspots_three_and_three() {
        let ids: Vec<&str> = functions().iter().map(|f| f.1).collect();
        assert_eq!(
            ids,
            [
                "com.sun.hotspot.functions.IsClassUnloadingEnabled",
                "com.sun.hotspot.functions.GetVirtualThread",
                "com.sun.hotspot.functions.GetCarrierThread",
            ]
        );
        let events: Vec<(JInt, &str)> = events().iter().map(|e| (e.0, e.1)).collect();
        assert_eq!(
            events,
            [
                (49, "com.sun.hotspot.events.ClassUnload"),
                (48, "com.sun.hotspot.events.VirtualThreadMount"),
                (47, "com.sun.hotspot.events.VirtualThreadUnmount"),
            ]
        );
    }

    /// gci i15/rb (`gci-i15rb-vm-default-path-changes-review-findings-FIXED-20261017`
    /// finding 3): both thread extension functions take ONE JNI entry before
    /// they decode their operand, so every raw `ObjectRef` they read
    /// afterwards (`thread_operand`'s answer, the registry's mirrors, the
    /// carrier's global) is read out of native. (Source pin: an in-native
    /// caller cannot be staged in a unit test of this module.)
    #[test]
    fn gci_i15rb_thread_extensions_hold_one_entry_across_the_operand() {
        let src = include_str!("extensions.rs").replace("\r\n", "\n");
        for (f, operand) in [
            ("extern \"C\" fn get_virtual_thread(", "thread_operand(&shared, carrier)"),
            ("extern \"C\" fn get_carrier_thread(", "thread_operand(&shared, vthread)"),
        ] {
            let start = src.find(f).expect(f);
            let rest = &src[start..];
            let body = &rest[..rest.find("\n}\n").expect("its end")];
            let entry = body.find("let _fx = jni::ForeignJniEntry::enter();").expect(f);
            let decode = body.find(operand).expect(operand);
            assert!(entry < decode, "{f}: the entry comes first");
        }
    }

    /// zt t23/x19 (review page item 28e): a startup agent's env, which has
    /// no VM in `Agent_OnLoad`, answers `IsClassUnloadingEnabled` with the
    /// bit its VM noted in the OnLoad scope (`-Xnoclassgc`: `JNI_FALSE`), as
    /// HotSpot reads the parsed flag in any phase. Controls: a VM that keeps
    /// unloading answers `JNI_TRUE`; nothing noted (a scope of a test, a
    /// live-phase env with no VM) answers `JNI_TRUE`; the kill arm
    /// (`CRATONVM_JVMTI_ONLOAD_CLASS_UNLOADING=0`) answers `JNI_TRUE` as before.
    #[test]
    fn zt23x19_an_onload_env_answers_its_vms_class_unloading_bit() {
        const KEY: &str = "CRATONVM_JVMTI_ONLOAD_CLASS_UNLOADING";
        let ask = |noted: Option<bool>, kill: Option<&str>| -> u8 {
            let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
            let _ = super::super::collect_startup_envs(|| {
                if let Some(on) = noted {
                    super::super::note_onload_class_unloading(on);
                }
                env = super::super::new_env_for_version(0x3001_0200).expect("a 1.2 env");
            });
            let mut out = 7u8;
            let code = cratonvm_types::flags::with_thread_overrides(&[(KEY, kill)], || {
                is_class_unloading_enabled(env.cast::<JvmtiNativeEnv>(), &mut out)
            });
            assert_eq!(code, 0, "noted {noted:?}, kill {kill:?}: JVMTI_ERROR_NONE");
            out
        };
        assert_eq!(ask(Some(false), None), 0, "-Xnoclassgc in Agent_OnLoad");
        assert_eq!(ask(Some(true), None), 1, "control: unloading on");
        assert_eq!(ask(None, None), 1, "control: nothing noted");
        assert_eq!(ask(Some(false), Some("0")), 1, "kill arm: JNI_TRUE as before");
        // The pure rule, and the switch is read only when a bit was noted.
        assert!(onload_class_unloading_answer(None, || panic!("not read")));
        assert!(!onload_class_unloading_answer(Some(false), || true));
        assert!(onload_class_unloading_answer(Some(false), || false));
    }
}
