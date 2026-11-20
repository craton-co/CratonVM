#[allow(unused_imports)]
use super::*;

/// Slot 89: `GetCapabilities(env, capabilities_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn get_capabilities(
    env: *mut JvmtiNativeEnv,
    out: *mut [u32; CAPABILITY_WORDS],
) -> JInt {
    match live_env(env) {
        Ok(env) => write_capabilities(out, env.state.lock().capabilities),
        Err(code) => code,
    }
}

/// Slot 140: `GetPotentialCapabilities(env, capabilities_ptr)` (OnLoad or
/// live phase).
pub(in crate::jvmti::native_env) extern "C" fn get_potential_capabilities(
    env: *mut JvmtiNativeEnv,
    out: *mut [u32; CAPABILITY_WORDS],
) -> JInt {
    match live_env(env) {
        Ok(env) if env.outside_live_phase() => ERR_WRONG_PHASE,
        Ok(env) => write_capabilities(out, env.potential()),
        Err(code) => code,
    }
}

/// Read the agent's `const jvmtiCapabilities*`.
pub(in crate::jvmti::native_env) fn read_capabilities(caps: *const [u32; CAPABILITY_WORDS]) -> Option<[u32; CAPABILITY_WORDS]> {
    if caps.is_null() {
        return None;
    }
    // SAFETY: the agent's non-null, 16-byte `jvmtiCapabilities`.
    Some(unsafe { caps.read_unaligned() })
}

/// Slot 142: `AddCapabilities(env, capabilities_ptr)`: all or nothing;
/// `JVMTI_ERROR_NOT_AVAILABLE` when one asked for is not potentially
/// available. OnLoad or live phase.
pub(in crate::jvmti::native_env) extern "C" fn add_capabilities(
    env: *mut JvmtiNativeEnv,
    caps: *const [u32; CAPABILITY_WORDS],
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    let Some(requested) = read_capabilities(caps) else {
        return ERR_NULL_POINTER;
    };
    if requested
        .iter()
        .zip(env.potential())
        .any(|(&r, p)| r & !p != 0)
    {
        return ERR_NOT_AVAILABLE;
    }
    {
        let mut state = env.state.lock();
        for (have, r) in state.capabilities.iter_mut().zip(requested) {
            *have |= r;
        }
    }
    // t1 wave 3, lane M: a live-phase `can_tag_objects` makes every LATER
    // compile of this VM keep its allocations (`DebuggerGates::c_agent_walks_heap`).
    // An OnLoad-phase env is bound to its VM later (`bind_startup_envs`
    // reads its capabilities then), so `vm()` may be `None` here.
    if has_cap(&requested, CAP_TAG_OBJECTS) {
        if let Some(shared) = env.vm() {
            note_live_heap_walker(&shared);
        }
    }
    ERR_NONE
}

/// t1 wave 5, lane M (`t1-V3-in-flight-compile-keeps-scalar-replacement-after-
/// live-can-tag-objects`): a live-phase env of `shared` acquired
/// `can_tag_objects`. Raise the gate the compile doors read
/// (`DebuggerGates::c_agent_walks_heap`), then, the first time only, refuse
/// every compile already in flight
/// (`JitCache::refuse_in_flight_for_object_observer`,
/// `CRATONVM_JIT_OBSERVER_GAIN_REFUSES_IN_FLIGHT`): its request may have read
/// the gate down and scalar-replaced allocations a heap walk must see.
/// HotSpot's `ciEnv::jvmti_state_changed`. The order matters: a compile that
/// begins after the record (its witness stamps a later invalidation epoch)
/// reads the raised gate. Called with no lock of this env held. Returns
/// whether the record was logged.
pub(in crate::jvmti::native_env) fn note_live_heap_walker(shared: &SharedVm) -> bool {
    if !shared.debug.debugger_gates.note_c_agent_walks_heap() {
        return false;
    }
    shared.jit.jit_cache.read().refuse_in_flight_for_object_observer()
}

/// Slot 143: `RelinquishCapabilities(env, capabilities_ptr)` (OnLoad or
/// live phase). Giving up the capability of an event delivered through the
/// VM's manager ([`MANAGER_EVENTS`]: `can_generate_breakpoint_events`,
/// `can_generate_exception_events`, and since wave 17 the method entry and
/// exit ones) disables that event (for every thread), on the VM too (wave
/// 14).
pub(in crate::jvmti::native_env) extern "C" fn relinquish_capabilities(
    env: *mut JvmtiNativeEnv,
    caps: *const [u32; CAPABILITY_WORDS],
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if env.outside_live_phase() {
        return ERR_WRONG_PHASE;
    }
    let Some(released) = read_capabilities(caps) else {
        return ERR_NULL_POINTER;
    };
    {
        let mut state = env.state.lock();
        for (have, r) in state.capabilities.iter_mut().zip(released) {
            *have &= !r;
        }
        for (event, capability) in MANAGER_EVENTS {
            if state.capabilities[0] & capability == 0 {
                state.disable_everywhere(event);
            }
        }
    }
    if let Some(shared) = env.vm() {
        for (event, _) in MANAGER_EVENTS {
            let _ = env.sync_event(&shared, event);
        }
    }
    ERR_NONE
}

/// Slot 127: `DisposeEnvironment(env)`: clears the breakpoints this env set,
/// unregisters its delivery, leaves its VM's list of native envs, and marks
/// it disposed (never freed; see the module doc).
pub(in crate::jvmti::native_env) extern "C" fn dispose_environment(env: *mut JvmtiNativeEnv) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    // Serialised with `sync_event`, which then finds the env disposed and
    // its delivery gone.
    let _serial = env.sync.lock();
    let (vm, delivery, breakpoints) = {
        let mut state = env.state.lock();
        state.disposed = true;
        state.enabled = 0;
        state.thread_enabled.clear();
        state.capabilities = [0; CAPABILITY_WORDS];
        (
            state.vm.as_ref().and_then(Weak::upgrade),
            state.delivery.take(),
            std::mem::take(&mut state.breakpoints),
        )
    };
    if let Some(shared) = vm.clone() {
        for (method, location) in breakpoints {
            let _ = rt::clear_breakpoint_for_vm(&shared, method, location);
        }
        let addr = env.addr();
        shared
            .debug
            .jvmti_env
            .lock()
            .c_table
            .native_envs
            .retain(|&listed| listed != addr);
        refresh_class_prepare_gate(&shared);
        events_more::refresh_event_gates(&shared);
    }
    // Detached, its enable leaves the manager's union flags (wave 14).
    if let Some((manager, delivery)) = delivery {
        let _ = manager.unregister_env(&delivery);
    }
    // And the native-call funnel's gate (wave 27), read after the detach.
    if let Some(shared) = vm {
        refresh_method_events_gate(&shared);
    }
    ERR_NONE
}

/// Slot 128: `GetErrorName(env, error, name_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn get_error_name(
    env: *mut JvmtiNativeEnv,
    error: JInt,
    name_ptr: *mut *mut u8,
) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if name_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let known = rt::JvmtiError::from_code(error);
    let name = if known as JInt == error {
        known.name()
    } else {
        match jdk25_only_error_name(error) {
            Some(name) => name,
            None => return ERR_ILLEGAL_ARGUMENT,
        }
    };
    let mem = allocate_modified_utf8(name);
    if mem.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *name_ptr = mem };
    ERR_NONE
}

/// The `jvmtiError` names of JDK 25's `jvmti.h` that `runtime::jvmti::JvmtiError`
/// does not list (interpreter round i2 wave 57, lane L1: `GetErrorName`
/// refused them with `JVMTI_ERROR_ILLEGAL_ARGUMENT`; MEASURED,
/// `L1W57JvmtiErrorNames`, HotSpot 25.0.3 names every one).
pub(in crate::jvmti::native_env) fn jdk25_only_error_name(error: JInt) -> Option<&'static str> {
    Some(match error {
        26 => "JVMTI_ERROR_INVALID_MODULE",
        72 => "JVMTI_ERROR_UNSUPPORTED_REDEFINITION_CLASS_ATTRIBUTE_CHANGED",
        73 => "JVMTI_ERROR_UNSUPPORTED_OPERATION",
        79 => "JVMTI_ERROR_UNMODIFIABLE_CLASS",
        80 => "JVMTI_ERROR_UNMODIFIABLE_MODULE",
        102 => "JVMTI_ERROR_INVALID_EVENT_TYPE",
        104 => "JVMTI_ERROR_NATIVE_METHOD",
        106 => "JVMTI_ERROR_CLASS_LOADER_UNSUPPORTED",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// What agents call first: phase, env storage, raw monitors, thread, thread
// group, class and field facts (interpreter round i1 wave 52, lane L1)
// ---------------------------------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// Probe: `tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.java`.

/// `jvmtiPhase` (`GetPhase`); the VM's own phases are `crate::debug`'s
/// `JVMTI_PHASE_*` (wave 55).
pub(in crate::jvmti::native_env) const PHASE_ONLOAD: JInt = 1;
pub(in crate::jvmti::native_env) const PHASE_DEAD: JInt = 8;

/// Slot 133: `GetPhase(env, phase_ptr)`: OnLoad while a startup agent's env
/// waits for its VM, then the VM's phase (`DebuggerGates::jvmti_phase`:
/// primordial until `System.initPhase1` returns, start until `VMInit`, live
/// after it, dead after `VMDeath`), dead once the VM is gone.
pub(in crate::jvmti::native_env) extern "C" fn get_phase(env: *mut JvmtiNativeEnv, phase_ptr: *mut JInt) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if phase_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // Wave 55: the VM's one phase value (primordial and dead only with
    // `DebuggerGates::c_jvmti_phases`).
    let phase = if env.state.lock().startup_pending {
        PHASE_ONLOAD
    } else {
        env.vm().map_or(PHASE_DEAD, |shared| {
            JInt::from(shared.debug.debugger_gates.jvmti_phase())
        })
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *phase_ptr = phase };
    ERR_NONE
}

/// Slot 147: `GetEnvironmentLocalStorage(env, data_ptr)`: the pointer
/// [`set_environment_local_storage`] stored, NULL before any.
pub(in crate::jvmti::native_env) extern "C" fn get_environment_local_storage(
    env: *mut JvmtiNativeEnv,
    data_ptr: *mut usize,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    if data_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let data = env.state.lock().local_storage;
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *data_ptr = data };
    ERR_NONE
}

/// Slot 148: `SetEnvironmentLocalStorage(env, data)`: any phase.
pub(in crate::jvmti::native_env) extern "C" fn set_environment_local_storage(env: *mut JvmtiNativeEnv, data: usize) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    env.state.lock().local_storage = data;
    ERR_NONE
}

/// The VM thread a `jthread` names for the thread-local storage: NULL is
/// the calling thread (`JVMTI_ERROR_UNATTACHED_THREAD` for one the VM does
/// not know); another must be alive (`JVMTI_ERROR_THREAD_NOT_ALIVE`).
///
/// Interpreter round i4 wave 95 (lane L1): `jvmti.xml` allows both storage
/// functions in the start phase (`phase="start"`), so the phase check is
/// [`JvmtiNativeEnv::start_or_live_vm`]'s, not the live phase's (they
/// answered `JVMTI_ERROR_WRONG_PHASE` to an agent's `VMStart` callback).
/// `null_out`: the caller's out-parameter is NULL; HotSpot's `jvmtiEnter`
/// wrapper answers `JVMTI_ERROR_NULL_POINTER` after the phase and before the
/// thread is looked at.
pub(in crate::jvmti::native_env) fn storage_thread(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    null_out: bool,
) -> Result<(&'static JvmtiNativeEnv, u64), JInt> {
    let env = live_env(env)?;
    let shared = env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    if null_out {
        return Err(ERR_NULL_POINTER);
    }
    let tid = if thread == 0 {
        let current = jni::current_jvm_thread_of(&shared).ok_or(ERR_UNATTACHED_THREAD)?;
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        unsafe { (*current).thread_id.0 }
    } else {
        thread_id_of(&shared, thread)?    };
    Ok((env, tid))}

/// Slot 102: `GetThreadLocalStorage(env, thread, data_ptr)` (wave 52): the
/// pointer [`set_thread_local_storage`] stored for the thread, NULL before
/// any.
pub(in crate::jvmti::native_env) extern "C" fn get_thread_local_storage(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    data_ptr: *mut usize,
) -> JInt {
    let (env, tid) = match storage_thread(env, thread, data_ptr.is_null()) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let data = env
        .state
        .lock()
        .thread_storage
        .iter()
        .find(|&&(t, _)| t == tid)
        .map_or(0, |&(_, data)| data);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *data_ptr = data };
    ERR_NONE
}

/// Slot 103: `SetThreadLocalStorage(env, thread, data)` (wave 52). The
/// pointer is the agent's; a thread's entry stays until replaced (a VM
/// thread id is not reused).
pub(in crate::jvmti::native_env) extern "C" fn set_thread_local_storage(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    data: usize,
) -> JInt {
    let (env, tid) = match storage_thread(env, thread, false) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let mut state = env.state.lock();
    state.thread_storage.retain(|&(t, _)| t != tid);
    if data != 0 {
        state.thread_storage.push((tid, data));
    }
    ERR_NONE
}

/// `can_get_bytecodes`, the 4th bit field (wave 52: `GetBytecodes`).
pub(in crate::jvmti::native_env) const CAN_GET_BYTECODES: u32 = 1 << 3;

/// Slot 75: `GetBytecodes(env, method, bytecode_count_ptr, bytecodes_ptr)`
/// (wave 52; `can_get_bytecodes`): the method's code array as the class
/// file has it, in an `Allocate` block; `JVMTI_ERROR_NATIVE_METHOD` for a
/// native method.
pub(in crate::jvmti::native_env) extern "C" fn get_bytecodes(
    env: *mut JvmtiNativeEnv,
    method: u64,
    bytecode_count_ptr: *mut JInt,
    bytecodes_ptr: *mut *mut u8,
) -> JInt {
    let shared = match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_BYTECODES) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(e) => match e.start_or_live_vm() {
            Some(shared) => shared,
            None => return ERR_WRONG_PHASE,        },
        Err(code) => return code,
    };
    let code = with_method(&shared, method, |_, m| {
        if m.is_native() {
            Err(ERR_NATIVE_METHOD)
        } else {
            Ok(m.code().map_or_else(Vec::new, |code| code.code.to_vec()))
        }
    });
    // Wave 56: a lambda proxy's method; wave 57: its spun code
    // ([`proxy_method_code`]).
    let code = code.or_else(|| proxy_method_code(&shared, method));
    let code = match code {
        Some(Ok(code)) => code,
        Some(Err(code)) => return code,
        None => return ERR_INVALID_METHODID,
    };
    write_id_array(bytecode_count_ptr, bytecodes_ptr, &code)
}

/// Slot 129: `GetJLocationFormat(env, format_ptr)`: `JVMTI_JLOCATION_JVMBCI`
/// (1), a `jlocation` being a bytecode index.
pub(in crate::jvmti::native_env) extern "C" fn get_jlocation_format(env: *mut JvmtiNativeEnv, format_ptr: *mut JInt) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if format_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *format_ptr = 1 };
    ERR_NONE
}

/// Slot 139: `GetTime(env, nanos_ptr)`: HotSpot's `os::javaTimeNanos`, the
/// clock `System.nanoTime` reads, so the two readings compare.
pub(in crate::jvmti::native_env) extern "C" fn get_time(env: *mut JvmtiNativeEnv, nanos_ptr: *mut i64) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if nanos_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let nanos = cratonvm_native_builtins::lang_system::java_time_nanos();
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *nanos_ptr = nanos };
    ERR_NONE
}

/// Slot 144: `GetAvailableProcessors(env, processor_count_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn get_available_processors(env: *mut JvmtiNativeEnv, count_ptr: *mut JInt) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if count_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let count = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *count_ptr = JInt::try_from(count).unwrap_or(JInt::MAX) };
    ERR_NONE
}

/// The VM properties a startup agent reads in `Agent_OnLoad` (wave 61),
/// before `SharedVm::new` builds the property map: the keys of
/// `vm_properties::VM_PROPERTIES` HotSpot has by then, with the initial values that map
/// gives them (one copy each, `vm::vm_init::vm_identity_properties` and the
/// `initial_*` functions), then the `-D` properties; each with whether
/// `SetSystemProperty` may write it.
pub(crate) fn onload_vm_properties(
    config: &crate::config::VmConfig,
) -> Vec<(String, String, bool)> {
    use crate::vm::vm_init as init;
    let mut out: Vec<(String, String, bool)> = Vec::new();
    let mut put = |key: &str, value: String| {
        let writable = is_writeable_vm_property(key);
        out.retain(|(k, _, _)| k != key);
        out.push((key.to_string(), value, writable));
    };
    for (key, value) in init::vm_identity_properties(config) {
        put(key, value);
    }
    let java_home = init::initial_java_home(config);
    put("java.class.path", init::initial_java_class_path(config));
    put(
        "java.library.path",
        init::initial_java_library_path(&java_home),
    );
    put(
        "sun.boot.library.path",
        init::initial_sun_boot_library_path(&java_home),
    );
    put("java.home", java_home.clone());
    for (key, value) in &config.system_properties {
        if !internal_property(key) {
            let value = init::command_line_property_value(&java_home, key, value);
            out.retain(|(k, _, _)| k != key);
            out.push((key.clone(), value, true));
        }
    }
    // Interpreter round i7 wave 135, lane L1b (`CRATONVM_VM_PROPERTY_LIST_ORDER`,
    // parity, default ON, `=0` off): HotSpot's list order, measured; a `-D`
    // of a list key goes back to the key's place (the stable sort keeps the
    // other `-D` ones in command-line order after the list).
    if config.vm_property_list_order {
        out.sort_by_key(|(k, _, _)| cratonvm_types::vm_properties::hotspot_list_sort_key(k));    }
    out
}

/// What a property function acts on: the OnLoad phase's properties
/// ([`StartupScope::properties`]) or the live VM's map.
pub(in crate::jvmti::native_env) enum PropertyPhase {
    OnLoad,
    Live(Arc<SharedVm>),
}

/// The phase of a property or class-search call: OnLoad (a startup env not
/// bound yet) or live; any other is `JVMTI_ERROR_WRONG_PHASE`.
pub(in crate::jvmti::native_env) fn property_phase(env: *mut JvmtiNativeEnv) -> Result<PropertyPhase, JInt> {
    let env = live_env(env)?;
    if env.state.lock().startup_pending {
        return Ok(PropertyPhase::OnLoad);
    }
    if env.outside_live_phase() {
        return Err(ERR_WRONG_PHASE);
    }
    env.vm().map(PropertyPhase::Live).ok_or(ERR_WRONG_PHASE)
}

/// The VM properties readable in the live phase, in HotSpot's list order
/// (`vm_properties::VM_PROPERTIES`, then the `-D` ones), with their values now.
pub(in crate::jvmti::native_env) fn live_vm_properties(shared: &SharedVm) -> Vec<(String, String)> {
    let props = shared.system_properties.read();
    let mut out: Vec<(String, String)> = Vec::new();
    let user = shared
        .config
        .system_properties
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| !internal_property(k));
    let mut keys: Vec<&str> = Vec::new();
    for key in vm_property_keys() {
        keys.push(key);
    }
    keys.extend(user);
    for key in keys {
        if out.iter().any(|(k, _)| k == key) {
            continue;
        }
        if let Some(value) = props.get(key) {
            out.push((key.to_string(), value.clone()));
        }
    }
    // Wave 135, lane L1b (`CRATONVM_VM_PROPERTY_LIST_ORDER`): HotSpot's
    // order -- the before-heap list, the `-D` ones, then
    // `java.vm.compressedOopsMode`.
    if shared.config.vm_property_list_order {
        out.sort_by_key(|(k, _)| cratonvm_types::vm_properties::hotspot_list_sort_key(k));    }
    out
}

/// The VM properties of `phase`, as [`live_vm_properties`] lists them.
pub(in crate::jvmti::native_env) fn vm_properties_of(phase: &PropertyPhase) -> Vec<(String, String)> {
    match phase {
        PropertyPhase::Live(shared) => live_vm_properties(shared),
        PropertyPhase::OnLoad => STARTUP_ENVS.with(|c| {
            c.borrow()
                .as_ref()
                .map(|scope| {
                    scope
                        .properties
                        .iter()
                        .map(|(k, v, _)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default()
        }),
    }
}

/// `text` as a NUL-terminated platform string (UTF-8) in an `Allocate`
/// block, or null when out of memory: the VM properties are platform
/// strings, not modified UTF-8.
pub(in crate::jvmti::native_env) fn allocate_c_string(text: &str) -> *mut u8 {
    let mem = allocate_block(text.len() + 1);
    if !mem.is_null() {
        // SAFETY: `mem` is a fresh block of `text.len() + 1` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), mem, text.len());
            mem.add(text.len()).write(0);
        }
    }
    mem
}

/// The platform string at `ptr`, which is not NULL.
///
/// # Safety
///
/// `ptr` points at a NUL-terminated string the agent passed.
pub(in crate::jvmti::native_env) unsafe fn agent_c_string(ptr: *const std::ffi::c_char) -> String {
    std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
}

/// Slot 130: `GetSystemProperties(env, count_ptr, property_ptr)` (wave 61;
/// OnLoad and live phases): the keys of the VM's property list, each an
/// `Allocate` block, in one more.
pub(in crate::jvmti::native_env) extern "C" fn get_system_properties(
    env: *mut JvmtiNativeEnv,
    count_ptr: *mut JInt,
    property_ptr: *mut *mut *mut u8,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    if count_ptr.is_null() || property_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let keys = vm_properties_of(&phase);
    let array =
        allocate_block(keys.len().max(1) * std::mem::size_of::<*mut u8>()).cast::<*mut u8>();
    if array.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    for (at, (key, _)) in keys.iter().enumerate() {
        let block = allocate_c_string(key);
        if block.is_null() {
            for done in 0..at {
                // SAFETY: each block below `at` was allocated above.
                unsafe { free_block(*array.add(done)) };
            }
            // SAFETY: allocated above.
            unsafe { free_block(array.cast()) };
            return ERR_OUT_OF_MEMORY;
        }
        // SAFETY: `array` has room for `keys.len()` pointers.
        unsafe { *array.add(at) = block };
    }
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *count_ptr = JInt::try_from(keys.len()).unwrap_or(JInt::MAX);
        *property_ptr = array;
    }
    ERR_NONE
}

/// Slot 131: `GetSystemProperty(env, property, value_ptr)` (wave 61; OnLoad
/// and live phases): the value of a VM property, an `Allocate` block;
/// `JVMTI_ERROR_NOT_AVAILABLE` for a key the VM does not list (MEASURED:
/// `user.dir`, `os.name`, `java.version`).
pub(in crate::jvmti::native_env) extern "C" fn get_system_property(
    env: *mut JvmtiNativeEnv,
    property: *const std::ffi::c_char,
    value_ptr: *mut *mut u8,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    if property.is_null() || value_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null string.
    let key = unsafe { agent_c_string(property) };
    let Some((_, value)) = vm_properties_of(&phase)
        .into_iter()
        .find(|(k, _)| *k == key)
    else {
        return ERR_NOT_AVAILABLE;
    };
    let block = allocate_c_string(&value);
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *value_ptr = block };
    ERR_NONE
}

/// Slot 132: `SetSystemProperty(env, property, value_ptr)` (wave 61; the
/// OnLoad phase only, MEASURED: the live phase answers `WRONG_PHASE` before
/// it reads its arguments): a writable VM property
/// (`vm_properties::is_writeable_vm_property`, or a `-D` one) takes `value_ptr` (NULL: unchanged), and the VM starts
/// with it ([`OnLoadEffects`]); any other key is `JVMTI_ERROR_NOT_AVAILABLE`.
pub(in crate::jvmti::native_env) extern "C" fn set_system_property(
    env: *mut JvmtiNativeEnv,
    property: *const std::ffi::c_char,
    value_ptr: *const std::ffi::c_char,
) -> JInt {
    match property_phase(env) {
        Ok(PropertyPhase::OnLoad) => {}
        Ok(PropertyPhase::Live(_)) => return ERR_WRONG_PHASE,
        Err(code) => return code,
    }
    if property.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null strings.
    let key = unsafe { agent_c_string(property) };
    let value = (!value_ptr.is_null()).then(|| unsafe { agent_c_string(value_ptr) });
    STARTUP_ENVS.with(|c| {
        let mut scope = c.borrow_mut();
        let Some(scope) = scope.as_mut() else {
            return ERR_NOT_AVAILABLE;
        };
        let Some(entry) = scope.properties.iter_mut().find(|(k, _, _)| *k == key) else {
            return ERR_NOT_AVAILABLE;
        };
        if !entry.2 {
            return ERR_NOT_AVAILABLE;
        }
        if let Some(value) = value {
            entry.1 = value.clone();
            scope.effects.properties.push((key, value));
        }
        ERR_NONE
    })
}

/// Slot 138: `GetTimerInfo(env, info_ptr)` (wave 61; any phase): the timer of
/// `GetTime`, `System.nanoTime`'s (MEASURED: `max=-1 forward=0 backward=0
/// kind=32`, `JVMTI_TIMER_ELAPSED`).
pub(in crate::jvmti::native_env) extern "C" fn get_timer_info(env: *mut JvmtiNativeEnv, info_ptr: *mut TimerInfoRow) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(TimerInfoRow {
            max_value: -1,
            may_skip_forward: 0,
            may_skip_backward: 0,
            kind: TIMER_ELAPSED,
            reserved1: 0,
            reserved2: 0,
        });
    }
    ERR_NONE
}

/// `JVMTI_TIMER_ELAPSED`: elapsed time.
pub(in crate::jvmti::native_env) const TIMER_ELAPSED: JInt = 32;

/// The object `object` names, read inside the calling thread's
/// foreign-thread entry, then `work` with a native context on the calling
/// thread (no collection can move it meanwhile: the caller is a registered
/// mutator inside a JNI call). The start or live phase.
/// `JVMTI_ERROR_INVALID_OBJECT` for NULL or a handle that names nothing.
pub(in crate::jvmti::native_env) fn with_object<R>(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    out_is_null: bool,
    work: impl FnOnce(&mut crate::vm::vm_exec::NativeContextImpl<'_>, ObjectRef) -> R,
) -> Result<R, JInt> {
    // Wave 94 (i54 stage 2): both callers are `phase="start"` functions.
    let shared = live_env(env)?.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    if object == 0 {
        return Err(ERR_INVALID_OBJECT);
    }
    if out_is_null {
        return Err(ERR_NULL_POINTER);
    }
    let current = jni::current_jvm_thread_of(&shared).ok_or(ERR_UNATTACHED_THREAD)?;
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(&shared);
    let obj = jni::jobject_to_obj(object);
    jni::restore_jni_context(prev);
    let obj = obj.ok_or(ERR_INVALID_OBJECT)?;
    let mut ctx = crate::vm::vm_exec::NativeContextImpl {
        shared: &shared,
        // SAFETY: the calling thread's own, live `JvmThread`, which this call
        // runs on and does not otherwise touch meanwhile.
        thread: unsafe { &mut *current },
    };
    Ok(work(&mut ctx, obj))
}

/// Slot 58: `GetObjectHashCode(env, object, hash_code_ptr)` (wave 61): the
/// object's identity hash, `System.identityHashCode`'s (MEASURED: the same
/// value, and not a `String`'s `hashCode()`).
pub(in crate::jvmti::native_env) extern "C" fn get_object_hash_code(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    hash_code_ptr: *mut JInt,
) -> JInt {
    let hash = with_object(env, object, hash_code_ptr.is_null(), |ctx, obj| {
        ctx.identity_hash_code(obj)
    });
    match hash {
        Ok(hash) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *hash_code_ptr = hash };
            ERR_NONE
        }
        Err(code) => code,
    }
}

/// Slot 154: `GetObjectSize(env, object, size_ptr)` (wave 61):
/// `Instrumentation.getObjectSize`'s answer
/// (`runtime::instrument::object_size_of`, which `libinstrument` asks this
/// function for on HotSpot).
pub(in crate::jvmti::native_env) extern "C" fn get_object_size(
    env: *mut JvmtiNativeEnv,
    object: JObject,
    size_ptr: *mut i64,
) -> JInt {
    let size = with_object(env, object, size_ptr.is_null(), |ctx, obj| {
        crate::runtime::instrument::object_size_of(&*ctx, obj)
    });
    match size {
        Ok(size) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *size_ptr = size };
            ERR_NONE
        }
        Err(code) => code,
    }
}

/// Is the file at `path` a zip archive, as HotSpot's
/// `ClassLoader::create_class_path_zip_entry` asks in the live phase (a
/// missing path, a directory and a plain file are refused: MEASURED,
/// `ILLEGAL_ARGUMENT` for each)? Its local-header or empty-archive
/// signature.
pub(in crate::jvmti::native_env) fn is_zip_file(path: &str) -> bool {
    use std::io::Read as _;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return false;
    }
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic).is_ok() && (magic == *b"PK\x03\x04" || magic == *b"PK\x05\x06")
}

/// Slots 149 / 151: `AddToBootstrapClassLoaderSearch` /
/// `AddToSystemClassLoaderSearch(env, segment)` (wave 61). In the OnLoad
/// phase the segment is taken as given, as HotSpot takes it: appended to the
/// bootstrap search, or to `java.class.path` and the application class path,
/// when the VM binds the env ([`OnLoadEffects`]). In the live phase it must
/// be a jar (`ILLEGAL_ARGUMENT` otherwise) and is appended at once, as
/// `Instrumentation.appendTo*ClassLoaderSearch` appends it
/// (`runtime::instrument`'s `appendToClassLoaderSearch0`).
pub(in crate::jvmti::native_env) fn add_to_class_loader_search(
    env: *mut JvmtiNativeEnv,
    segment: *const std::ffi::c_char,
    boot: bool,
) -> JInt {
    let phase = match property_phase(env) {
        Ok(phase) => phase,
        Err(code) => return code,
    };
    if segment.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null string.
    let path = unsafe { agent_c_string(segment) };
    match phase {
        PropertyPhase::OnLoad => STARTUP_ENVS.with(|c| {
            let mut scope = c.borrow_mut();
            let Some(scope) = scope.as_mut() else {
                return ERR_WRONG_PHASE;
            };
            if boot {
                scope.effects.boot_segments.push(path);
                return ERR_NONE;
            }
            let separator = if cfg!(windows) { ';' } else { ':' };
            if let Some(entry) = scope
                .properties
                .iter_mut()
                .find(|(k, _, _)| k == "java.class.path")
            {
                if !entry.1.is_empty() {
                    entry.1.push(separator);
                }
                entry.1.push_str(&path);
                scope
                    .effects
                    .properties
                    .push(("java.class.path".to_string(), entry.1.clone()));
            }
            scope.effects.system_segments.push(path);
            ERR_NONE
        }),
        PropertyPhase::Live(shared) => {
            if !is_zip_file(&path) {
                return ERR_ILLEGAL_ARGUMENT;
            }
            let segments = [path];
            let mut cm = shared.classes.class_manager.write();
            if boot {
                cm.extend_bootstrap_classpath(&segments);
            } else {
                cm.extend_application_classpath(&segments);
            }
            ERR_NONE
        }
    }
}

pub(in crate::jvmti::native_env) extern "C" fn add_to_bootstrap_class_loader_search(
    env: *mut JvmtiNativeEnv,
    segment: *const std::ffi::c_char,
) -> JInt {
    add_to_class_loader_search(env, segment, true)
}

pub(in crate::jvmti::native_env) extern "C" fn add_to_system_class_loader_search(
    env: *mut JvmtiNativeEnv,
    segment: *const std::ffi::c_char,
) -> JInt {
    add_to_class_loader_search(env, segment, false)
}

/// Apply what the startup agents did in `Agent_OnLoad` to `shared`, which
/// now exists ([`OnLoadEffects`], from [`bind_startup_envs`]; wave 61).
pub(in crate::jvmti::native_env) fn apply_onload_effects(shared: &SharedVm, effects: OnLoadEffects) {
    if let Some(interval) = effects.heap_sampling_interval {
        shared.debug.jvmti_env.lock().heap_sampling_interval = interval;
    }
    if !effects.properties.is_empty() {
        let mut props = shared.system_properties.write();
        for (key, value) in effects.properties {
            props.insert(key, value);
        }
    }
    if !effects.boot_segments.is_empty() {
        shared
            .classes
            .class_manager
            .write()
            .extend_bootstrap_classpath(&effects.boot_segments);
    }
    if !effects.system_segments.is_empty() {
        shared
            .classes
            .class_manager
            .write()
            .extend_application_classpath(&effects.system_segments);
    }
}

// --- Raw monitors (slots 31-37) --------------------------------------------

/// `JVMTI_ERROR_INVALID_MONITOR` / `NOT_MONITOR_OWNER`.
pub(in crate::jvmti::native_env) const ERR_INVALID_MONITOR: JInt = 50;
pub(in crate::jvmti::native_env) const ERR_NOT_MONITOR_OWNER: JInt = 51;

/// A `jrawMonitorID`: a reentrant monitor with wait and notify, owned by an
/// OS thread (a raw monitor may be entered by any thread, attached or not,
/// and in `Agent_OnLoad`). Freed by `DestroyRawMonitor`.
pub(in crate::jvmti::native_env) struct RawMonitor {
    /// Tells a live monitor from a stray pointer (`INVALID_MONITOR`).
    pub(in crate::jvmti::native_env) magic: u64,
    pub(in crate::jvmti::native_env) state: parking_lot::Mutex<RawMonitorState>,
    /// Signalled when the monitor becomes free.
    pub(in crate::jvmti::native_env) entry: parking_lot::Condvar,
    /// Signalled by `RawMonitorNotify` / `NotifyAll`.
    pub(in crate::jvmti::native_env) waiters: parking_lot::Condvar,
}

pub(in crate::jvmti::native_env) struct RawMonitorState {
    pub(in crate::jvmti::native_env) owner: Option<std::thread::ThreadId>,
    /// Entries of the owner not yet exited.
    pub(in crate::jvmti::native_env) count: u32,
}

/// [`RawMonitor::magic`] of a live monitor ("RAWMONIT").
pub(in crate::jvmti::native_env) const RAW_MONITOR_MAGIC: u64 = 0x5241_574D_4F4E_4954;

/// The monitor a `jrawMonitorID` names, or `INVALID_MONITOR`.
pub(in crate::jvmti::native_env) fn raw_monitor<'a>(monitor: *const RawMonitor) -> Result<&'a RawMonitor, JInt> {
    if monitor.is_null() {
        return Err(ERR_INVALID_MONITOR);
    }
    // SAFETY: an id `CreateRawMonitor` handed out and not destroyed (the
    // spec makes any other value the agent's error; the magic catches the
    // common ones).
    let monitor = unsafe { &*monitor };
    if monitor.magic != RAW_MONITOR_MAGIC {
        return Err(ERR_INVALID_MONITOR);
    }
    Ok(monitor)
}

/// Run `block`, which may wait for another thread, GC-safe when the env has
/// a VM ([`gc_safe`]); plainly in `Agent_OnLoad`.
pub(in crate::jvmti::native_env) fn raw_monitor_block<R>(env: &JvmtiNativeEnv, block: impl FnOnce() -> R) -> R {
    match env.vm() {
        Some(shared) => gc_safe(&shared, block),
        None => block(),
    }
}

/// gci i47w lane oz47 (`vi12-ev.list` T: every row a `[stw-stall]` hang):
/// take `raw`'s inner lock without stalling a stop-the-world pause.
///
/// A waiter that wakes in `RawMonitorWait` (or an enterer woken in
/// [`raw_monitor_acquire`]) re-takes this lock INSIDE its GC-safe region and
/// keeps it while `end_blocking_region` waits for a pause to finish. A thread
/// that then reached the lock with a plain `lock()` blocked counted and
/// running, so the pause never got its arrival and the holder never got out:
/// the probe's four `ThreadStart` callbacks (`RawMonitorEnter` / `Wait(1)`)
/// beside a collecting thread deadlocked every run. HotSpot's
/// `JvmtiRawMonitor` blocks a contended entry `_thread_blocked`. So: the
/// uncontended `try_lock` as before, and a contended one GC-safe
/// ([`raw_monitor_block`]); both modes, every collector.
pub(in crate::jvmti::native_env) fn lock_raw_monitor_state<'a>(
    env: &JvmtiNativeEnv,
    raw: &'a RawMonitor,
) -> parking_lot::MutexGuard<'a, RawMonitorState> {
    if let Some(state) = raw.state.try_lock() {
        return state;
    }
    raw_monitor_block(env, || raw.state.lock())
}

/// Slot 31: `CreateRawMonitor(env, name, monitor_ptr)` (OnLoad and live
/// phases, as HotSpot allows; the start phase too).
pub(in crate::jvmti::native_env) extern "C" fn create_raw_monitor(
    env: *mut JvmtiNativeEnv,
    name: *const std::ffi::c_char,
    monitor_ptr: *mut *mut RawMonitor,
) -> JInt {
    if let Err(code) = live_env(env) {
        return code;
    }
    if name.is_null() || monitor_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let monitor = Box::new(RawMonitor {
        magic: RAW_MONITOR_MAGIC,
        state: parking_lot::Mutex::new(RawMonitorState {
            owner: None,
            count: 0,
        }),
        entry: parking_lot::Condvar::new(),
        waiters: parking_lot::Condvar::new(),
    });
    // SAFETY: the agent's non-null out-parameter. OWNERSHIP: freed by
    // `DestroyRawMonitor`.
    unsafe { *monitor_ptr = Box::into_raw(monitor) };
    ERR_NONE
}

/// Slot 32: `DestroyRawMonitor(env, monitor)`: `NOT_MONITOR_OWNER` while
/// another thread owns it; the owner may destroy it (HotSpot exits it
/// first).
pub(in crate::jvmti::native_env) extern "C" fn destroy_raw_monitor(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    {
        let state = lock_raw_monitor_state(env, raw);
        if state
            .owner
            .is_some_and(|owner| owner != std::thread::current().id())
        {
            return ERR_NOT_MONITOR_OWNER;
        }
    }
    // SAFETY: an id `CreateRawMonitor` made from a `Box`; the magic is
    // cleared first so a later use of the stale id is likelier refused.
    unsafe {
        std::ptr::addr_of_mut!((*monitor).magic).write(0);
        drop(Box::from_raw(monitor));
    }
    ERR_NONE
}

/// Take `raw` for the calling thread, waiting while another owns it
/// (`state` is its lock, held).
pub(in crate::jvmti::native_env) fn raw_monitor_acquire(
    env: &JvmtiNativeEnv,
    raw: &RawMonitor,
    state: &mut parking_lot::MutexGuard<'_, RawMonitorState>,
    count: u32,
) {
    let me = std::thread::current().id();
    if state.owner.is_some() {
        raw_monitor_block(env, || {
            while state.owner.is_some() {
                raw.entry.wait(state);
            }
        });
    }
    state.owner = Some(me);
    state.count = count;
}

/// Slot 33: `RawMonitorEnter(env, monitor)`: reentrant.
pub(in crate::jvmti::native_env) extern "C" fn raw_monitor_enter(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let mut state = lock_raw_monitor_state(env, raw);
    if state.owner == Some(std::thread::current().id()) {
        state.count = state.count.saturating_add(1);
        return ERR_NONE;
    }
    raw_monitor_acquire(env, raw, &mut state, 1);
    ERR_NONE
}

/// Slot 34: `RawMonitorExit(env, monitor)`.
pub(in crate::jvmti::native_env) extern "C" fn raw_monitor_exit(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let mut state = lock_raw_monitor_state(env, raw);
    if state.owner != Some(std::thread::current().id()) {
        return ERR_NOT_MONITOR_OWNER;
    }
    state.count -= 1;
    if state.count == 0 {
        state.owner = None;
        raw.entry.notify_one();
    }
    ERR_NONE
}

/// Slot 35: `RawMonitorWait(env, monitor, millis)`: releases the monitor
/// entirely, waits for a notify or `millis` (none when `millis <= 0`), and
/// takes it back with its entry count.
pub(in crate::jvmti::native_env) extern "C" fn raw_monitor_wait(
    env: *mut JvmtiNativeEnv,
    monitor: *mut RawMonitor,
    millis: i64,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let mut state = lock_raw_monitor_state(env, raw);
    if state.owner != Some(std::thread::current().id()) {
        return ERR_NOT_MONITOR_OWNER;
    }
    let count = state.count;
    state.owner = None;
    state.count = 0;
    raw.entry.notify_one();
    raw_monitor_block(env, || match u64::try_from(millis) {
        Ok(ms) if ms > 0 => {
            let _ = raw
                .waiters
                .wait_for(&mut state, std::time::Duration::from_millis(ms));
        }
        _ => raw.waiters.wait(&mut state),
    });
    raw_monitor_acquire(env, raw, &mut state, count);
    ERR_NONE
}

/// Slots 36 / 37: `RawMonitorNotify` / `RawMonitorNotifyAll(env, monitor)`.
pub(in crate::jvmti::native_env) fn raw_monitor_notify(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor, all: bool) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let raw = match raw_monitor(monitor) {
        Ok(raw) => raw,
        Err(code) => return code,
    };
    let state = lock_raw_monitor_state(env, raw);
    if state.owner != Some(std::thread::current().id()) {
        return ERR_NOT_MONITOR_OWNER;
    }
    if all {
        raw.waiters.notify_all();
    } else {
        raw.waiters.notify_one();
    }
    ERR_NONE
}

pub(in crate::jvmti::native_env) extern "C" fn raw_monitor_notify_one(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    raw_monitor_notify(env, monitor, false)
}

pub(in crate::jvmti::native_env) extern "C" fn raw_monitor_notify_all(env: *mut JvmtiNativeEnv, monitor: *mut RawMonitor) -> JInt {
    raw_monitor_notify(env, monitor, true)
}

// --- Threads and thread groups (slots 9, 13, 14) ---------------------------

/// `jvmtiThreadInfo`: `{ char* name; jint priority; jboolean is_daemon;
/// jthreadGroup thread_group; jobject context_class_loader; }`, 32 bytes.
#[repr(C)]
pub(in crate::jvmti::native_env) struct ThreadInfoRow {
    pub(in crate::jvmti::native_env) name: *mut u8,
    pub(in crate::jvmti::native_env) priority: JInt,
    pub(in crate::jvmti::native_env) is_daemon: u8,
    pub(in crate::jvmti::native_env) thread_group: JObject,
    pub(in crate::jvmti::native_env) context_class_loader: JObject,
}

/// `jvmtiThreadGroupInfo`: `{ jthreadGroup parent; char* name; jint
/// max_priority; jboolean is_daemon; }`, 24 bytes.
#[repr(C)]
pub(in crate::jvmti::native_env) struct ThreadGroupInfoRow {
    pub(in crate::jvmti::native_env) parent: JObject,
    pub(in crate::jvmti::native_env) name: *mut u8,
    pub(in crate::jvmti::native_env) max_priority: JInt,
    pub(in crate::jvmti::native_env) is_daemon: u8,
}

/// `JVMTI_THREAD_NORM_PRIORITY`, a virtual thread's.
pub(in crate::jvmti::native_env) const NORM_PRIORITY: JInt = 5;

/// Run `read` on the calling thread's own `JvmThread` (a mutator: a JNI
/// native, an event callback, an attached agent thread) inside its
/// foreign-thread entry and in `shared`'s JNI context, so the handles it
/// decodes and makes are the caller's local references.
/// `JVMTI_ERROR_UNATTACHED_THREAD` for a thread the VM does not know.
pub(in crate::jvmti::native_env) fn on_calling_thread<R>(
    shared: &SharedVm,
    read: impl FnOnce(&mut crate::vm::vm_exec::NativeContextImpl<'_>) -> R,
) -> Result<R, JInt> {
    let current = jni::current_jvm_thread_of(shared).ok_or(ERR_UNATTACHED_THREAD)?;
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    // gci i18/na: this context is not under `safe_native_call_impl` (an
    // agent's C code called the JVMTI function), so its allocations used to
    // take the infallible arm, which aborts the process on Generational and
    // ZGC (`RedefineClasses` / `RetransformClasses` allocate). Inside the
    // scope a refusal unwinds no further than here -- never into the agent's
    // C frames -- and answers `JVMTI_ERROR_OUT_OF_MEMORY`, as HotSpot's
    // functions do when an allocation fails.
    let out = crate::runtime::native_oom::catch_alloc_oom_on(
        // SAFETY: the calling thread's own, live `JvmThread`, used only by
        // it for this call (as the event context installs it).
        unsafe { &mut *current },
        |thread| {
            let mut ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
            read(&mut ctx)
        },
    );
    jni::restore_jni_context(prev);
    out.map_err(|_| ERR_OUT_OF_MEMORY)
}

/// A string field of `obj` as text (NULL and a non-string: `None`).
pub(in crate::jvmti::native_env) fn string_field(
    ctx: &crate::vm::vm_exec::NativeContextImpl<'_>,
    obj: ObjectRef,
    slot: Option<usize>,
) -> Option<String> {
    match slot.map(|s| ctx.get_field(obj, s)) {
        Some(Value::Object(Some(s))) => crate::vm::read_java_string(&ctx.shared.mem.heap, s),
        _ => None,
    }
}

/// What `GetThreadInfo` answers of `thread_obj`, as HotSpot reads it
/// (`JvmtiEnv::GetThreadInfo`): `Thread.name`, the `FieldHolder`'s
/// priority, daemon bit and group (a terminated thread's group is NULL), the
/// context class loader; a virtual thread is a daemon of normal priority in
/// `Thread$Constants.VTHREAD_GROUP`. The fields are `java.lang.Thread`'s own
/// (`vm_exec::thread_field_owner`), not a subclass's of the same name.
/// Without a `FieldHolder` (a `Thread` of another layout) the registry's
/// daemon bit and the normal priority answer.
pub(in crate::jvmti::native_env) fn thread_info_of(
    ctx: &crate::vm::vm_exec::NativeContextImpl<'_>,
    thread_obj: ObjectRef,
    terminated: bool,
) -> (String, JInt, bool, Option<ObjectRef>, Option<ObjectRef>) {
    let shared = ctx.shared;
    let (name_slot, holder_slot, loader_slot, is_virtual) = {
        let cm = shared.classes.class_manager.read();
        let class_id = shared.mem.heap.class_id_of(thread_obj);
        let owner = crate::vm::vm_exec::thread_field_owner(shared, &cm.class_store, class_id);
        let slot = |name: &str| {
            crate::vm::vm_exec::resolve_field_index_in_hierarchy(owner, name, &cm.class_store)
        };
        (
            slot("name"),
            slot("holder"),
            slot("contextClassLoader"),
            cm.is_subclass_of_by_name(class_id, "java/lang/BaseVirtualThread"),
        )
    };
    let name = string_field(ctx, thread_obj, name_slot).unwrap_or_default();
    let loader = match loader_slot.map(|s| ctx.get_field(thread_obj, s)) {
        Some(Value::Object(loader)) => loader,
        _ => None,
    };
    if is_virtual {
        let group = if terminated {
            None
        } else {
            ctx.class_id_by_name("java/lang/Thread$Constants")
                .filter(|&class_id| ctx.is_class_initialized(class_id))
                .and_then(|class_id| {
                    let index = ctx.static_field_index_by_name(class_id, "VTHREAD_GROUP")?;
                    match ctx.get_static_field(class_id, index) {
                        Value::Object(group) => group,
                        _ => None,
                    }
                })
        };
        return (name, NORM_PRIORITY, true, group, loader);
    }
    let holder = match holder_slot.map(|s| ctx.get_field(thread_obj, s)) {
        Some(Value::Object(Some(holder))) => Some(holder),
        _ => None,
    };
    let (priority, daemon) = match holder {
        Some(holder) => (
            match ctx.get_field_by_name(holder, "priority") {
                Value::Int(p) => p,
                _ => NORM_PRIORITY,
            },
            matches!(ctx.get_field_by_name(holder, "daemon"), Value::Int(d) if d != 0),
        ),
        None => {
            let registry = &shared.threads.thread_registry;
            let daemon = crate::vm::vm_exec::read_java_thread_tid(shared, thread_obj)
                .and_then(|java_tid| registry.find_thread_id_by_java_tid(java_tid))
                .is_some_and(|tid| registry.is_daemon(tid));
            (NORM_PRIORITY, daemon)
        }
    };
    let group = if terminated {
        None
    } else {
        crate::debug::inspect::group_of_thread(ctx, thread_obj)    };
    (name, priority, daemon, group, loader)}

/// Slot 9: `GetThreadInfo(env, thread, info_ptr)`: a NULL `thread` is the
/// calling thread. A thread not started yet or terminated is answered too,
/// as HotSpot answers it from the `Thread` object (a terminated one with a
/// NULL group). The name is an `Allocate` block; the group and the loader
/// are local references.
pub(in crate::jvmti::native_env) extern "C" fn get_thread_info(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    info_ptr: *mut ThreadInfoRow,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Interpreter round i5 wave 101, lane L1: HotSpot's generated entry
    // (`jvmtiEnter`) pre-checks the out-parameter; the thread, a `noconvert`
    // parameter, is checked by the function after it (`GetThreadState` here
    // already did so). A NULL `info_ptr` with a bad handle answered
    // `INVALID_THREAD` where HotSpot answers `NULL_POINTER`.
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let terminated = if thread == 0 {
        false
    } else {
        match lookup_thread(&shared, thread) {
            Ok(lookup) => matches!(lookup, ThreadLookup::Dead),
            Err(code) => return code,
        }
    };
    let answer = on_calling_thread(&shared, |ctx| {
        let thread_obj = if thread == 0 {
            let tid = ctx.thread.thread_id;
            shared.threads.thread_registry.java_thread_obj(tid)
        } else {
            jni::jobject_to_obj(thread)
        };
        let thread_obj = thread_obj
            .filter(|&obj| is_thread_object(&shared, obj))
            .ok_or(ERR_INVALID_THREAD)?;
        let (name, priority, daemon, group, loader) = thread_info_of(ctx, thread_obj, terminated);
        let local = |obj: Option<ObjectRef>| obj.map_or(0, jni::new_local_handle);
        Ok((name, priority, daemon, local(group), local(loader)))
    });
    let (name, priority, daemon, group, loader) = match answer {
        Ok(Ok(facts)) => facts,
        Ok(Err(code)) | Err(code) => return code,
    };
    let name = allocate_modified_utf8(&name);
    if name.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(ThreadInfoRow {
            name,
            priority,
            is_daemon: u8::from(daemon),
            thread_group: group,
            context_class_loader: loader,
        });
    }
    ERR_NONE
}

/// Is `obj` a `java.lang.ThreadGroup`?
pub(in crate::jvmti::native_env) fn is_thread_group_object(shared: &SharedVm, obj: ObjectRef) -> bool {
    let class_id = shared.mem.heap.class_id_of(obj);
    shared
        .classes
        .class_manager
        .read()
        .is_subclass_of_by_name(class_id, "java/lang/ThreadGroup")
}

/// Slot 13: `GetTopThreadGroups(env, group_count_ptr, groups_ptr)`: the
/// root of the VM's thread groups (`system`), found from the live threads'
/// groups, as local references in an `Allocate`d array.
pub(in crate::jvmti::native_env) extern "C" fn get_top_thread_groups(
    env: *mut JvmtiNativeEnv,
    group_count_ptr: *mut JInt,
    groups_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if group_count_ptr.is_null() || groups_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let roots = on_calling_thread(&shared, |ctx| {
        let threads = shared
            .threads
            .thread_registry
            .alive_thread_objects(usize::MAX);
        let mut roots: Vec<ObjectRef> = Vec::new();
        for thread_obj in threads {
            let Some(group) = crate::debug::inspect::group_of_thread(ctx, thread_obj) else {
                continue;
            };
            let root = crate::debug::inspect::group_root(ctx, group);
            if !roots.iter().any(|r| r.as_ptr() == root.as_ptr()) {
                roots.push(root);
            }
        }
        roots
            .into_iter()
            .map(jni::new_local_handle)
            .collect::<Vec<JObject>>()
    });
    let roots = match roots {
        Ok(roots) => roots,
        Err(code) => return code,
    };
    let Ok(count) = JInt::try_from(roots.len()) else {
        return ERR_INTERNAL;
    };
    let block =
        allocate_block(roots.len().max(1) * std::mem::size_of::<JObject>()).cast::<JObject>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `block` holds `roots.len()` handles; the out-parameters are
    // the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(roots.as_ptr(), block, roots.len());
        *group_count_ptr = count;
        *groups_ptr = block;
    }
    ERR_NONE
}

/// Slot 14: `GetThreadGroupInfo(env, group, info_ptr)`: the group's
/// parent, name, `maxPriority` and daemon bit (`ThreadGroup`'s fields).
/// `JVMTI_ERROR_INVALID_THREAD_GROUP` for a handle naming no group.
pub(in crate::jvmti::native_env) extern "C" fn get_thread_group_info(
    env: *mut JvmtiNativeEnv,
    group: JObject,
    info_ptr: *mut ThreadGroupInfoRow,
) -> JInt {
    const ERR_INVALID_THREAD_GROUP: JInt = 11;
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let answer = on_calling_thread(&shared, |ctx| {
        let group = jni::jobject_to_obj(group)
            .filter(|&obj| is_thread_group_object(&shared, obj))
            .ok_or(ERR_INVALID_THREAD_GROUP)?;
        let name = match ctx.get_field_by_name(group, "name") {
            Value::Object(Some(s)) => crate::vm::read_java_string(&shared.mem.heap, s),
            _ => None,
        };
        let max_priority = match ctx.get_field_by_name(group, "maxPriority") {
            Value::Int(p) => p,
            _ => 10,
        };
        let daemon = matches!(ctx.get_field_by_name(group, "daemon"), Value::Int(d) if d != 0);
        let parent =
            crate::debug::inspect::group_parent(ctx, group).map_or(0, jni::new_local_handle);
        Ok((parent, name.unwrap_or_default(), max_priority, daemon))
    });
    let (parent, name, max_priority, daemon) = match answer {
        Ok(Ok(facts)) => facts,
        Ok(Err(code)) | Err(code) => return code,
    };
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let name = allocate_modified_utf8(&name);
    if name.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(ThreadGroupInfoRow {
            parent,
            name,
            max_priority,
            is_daemon: u8::from(daemon),
        });
    }
    ERR_NONE
}

// --- Classes (slots 50, 51, 53-57, 145) -------------------------------------

/// `can_get_source_file_name`, the 12th bit field (wave 52:
/// `GetSourceFileName`), and `can_get_synthetic_attribute`, the 5th
/// (`IsFieldSynthetic`, `IsMethodSynthetic`). HotSpot grants both in every
/// phase.
pub(in crate::jvmti::native_env) const CAN_GET_SOURCE_FILE_NAME: u32 = 1 << 11;
pub(in crate::jvmti::native_env) const CAN_GET_SYNTHETIC_ATTRIBUTE: u32 = 1 << 4;
/// `JVMTI_ERROR_INVALID_FIELDID`.
pub(in crate::jvmti::native_env) const ERR_INVALID_FIELDID: JInt = 25;
/// `ACC_INTERFACE`.
pub(in crate::jvmti::native_env) const ACC_INTERFACE: u16 = 0x0200;
/// `ACC_SYNTHETIC`.
pub(in crate::jvmti::native_env) const ACC_SYNTHETIC: u16 = 0x1000;

/// Is `name` a primitive type's class (`int`, ..., `void`)?
pub(in crate::jvmti::native_env) fn is_primitive_class_name(name: &str) -> bool {
    matches!(
        name,
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
    )
}

/// Store one `jint` out-parameter.
pub(in crate::jvmti::native_env) fn write_jint(ptr: *mut JInt, value: Result<JInt, JInt>) -> JInt {
    let value = match value {
        Ok(value) => value,
        Err(code) => return code,
    };
    if ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *ptr = value };
    ERR_NONE
}

/// Store one `jboolean` out-parameter.
pub(in crate::jvmti::native_env) fn write_jboolean(ptr: *mut u8, value: Result<bool, JInt>) -> JInt {
    let value = match value {
        Ok(value) => value,
        Err(code) => return code,
    };
    if ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *ptr = u8::from(value) };
    ERR_NONE
}

/// `ids` in a fresh `Allocate` block, with its count, into the agent's
/// out-parameters.
pub(in crate::jvmti::native_env) fn write_id_array<T: Copy>(count_ptr: *mut JInt, array_ptr: *mut *mut T, ids: &[T]) -> JInt {
    if count_ptr.is_null() || array_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Ok(count) = JInt::try_from(ids.len()) else {
        return ERR_INTERNAL;
    };
    let Some(bytes) = ids.len().max(1).checked_mul(std::mem::size_of::<T>()) else {
        return ERR_OUT_OF_MEMORY;
    };
    let block = allocate_block(bytes).cast::<T>();
    if block.is_null() {
        return ERR_OUT_OF_MEMORY;
    }
    // SAFETY: `block` holds `ids.len()` elements, 16-aligned; the
    // out-parameters are the agent's, non-null.
    unsafe {
        std::ptr::copy_nonoverlapping(ids.as_ptr(), block, ids.len());
        *count_ptr = count;
        *array_ptr = block;
    }
    ERR_NONE
}

/// Slot 50: `GetSourceFileName(env, klass, source_name_ptr)`
/// (`can_get_source_file_name`): the `SourceFile` attribute;
/// `JVMTI_ERROR_ABSENT_INFORMATION` without one, and for an array or a
/// primitive class.
pub(in crate::jvmti::native_env) extern "C" fn get_source_file_name(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    source_name_ptr: *mut *mut u8,
) -> JInt {
    match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_SOURCE_FILE_NAME) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(_) => {}
        Err(code) => return code,
    }
    // Wave 55: a lambda proxy class has none (measured 101).
    // Wave 56: a primitive class has none (measured 101).
    let source = match with_jvmti_class(
        env,
        klass,
        |_, class| class.source_file.clone(),
        |_, _| None,
        |_| None,
    ) {
        Ok(source) => source,
        Err(code) => return code,
    };
    if source_name_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(source) = source else {
        return ERR_ABSENT_INFORMATION;
    };
    if write_modified_utf8_outs(&[(source_name_ptr, Some(&source))]) {
        ERR_NONE
    } else {
        ERR_OUT_OF_MEMORY
    }
}

/// Slot 51: `GetClassModifiers(env, klass, modifiers_ptr)`: HotSpot 25's
/// answer, as JDWP's `ReferenceType.Modifiers` gives it
/// (`debug::jdwp_class_modifiers`: the `InnerClasses` flags of a nested
/// class, with `ACC_SUPER` for every class and interface, an array
/// `0x431`); a primitive class is `public final abstract` (`0x411`).
pub(in crate::jvmti::native_env) extern "C" fn get_class_modifiers(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    modifiers_ptr: *mut JInt,
) -> JInt {
    let modifiers = with_jvmti_class(
        env,
        klass,
        |_, class| {
            if is_primitive_class_name(&class.name) {
                PRIMITIVE_CLASS_MODIFIERS
            } else {
                JInt::try_from(crate::debug::jdwp_class_modifiers(class)).unwrap_or(0)
            }
        },
        // Wave 55: a lambda proxy class.
        |_, _| LAMBDA_PROXY_MODIFIERS,
        // Wave 56: a primitive class's mirror.
        |_| PRIMITIVE_CLASS_MODIFIERS,
    );
    write_jint(modifiers_ptr, modifiers)
}

/// The `jfieldID` bit that marks a static field (wave 52): JNI's encoding
/// `(declaring class id << 32) | index` gives a static field its index among
/// the class's static fields and an instance field its absolute slot, so the
/// two collide. `GetStaticFieldID` sets it too, and JNI's decode drops it
/// (`jni::decode_field_id`), so an id this table hands out works with
/// `GetStatic<Type>Field`.
pub(in crate::jvmti::native_env) const STATIC_FIELD_ID: u64 = jni::STATIC_FIELD_ID_MARK;

/// The `jfieldID`s of `class`'s declared fields, in class-file order.
pub(in crate::jvmti::native_env) fn declared_field_ids(class: &crate::classloading::Class) -> Vec<u64> {
    let class_half = u64::from(class.id.as_u32()) << 32;
    let (mut statics, mut instances) = (0u64, 0u64);
    let first = u64::try_from(class.first_field_index).unwrap_or(0);
    class
        .fields
        .iter()
        .map(|field| {
            if field.is_static() {
                statics += 1;
                STATIC_FIELD_ID | class_half | (statics - 1)
            } else {
                instances += 1;
                class_half | (first + instances - 1)
            }
        })
        .collect()
}

/// `JVMTI_ERROR_CLASS_NOT_PREPARED`.
pub(in crate::jvmti::native_env) const ERR_CLASS_NOT_PREPARED: JInt = 22;
/// `JVMTI_CLASS_STATUS_PREPARED`.
pub(in crate::jvmti::native_env) const CLASS_STATUS_PREPARED: JInt = 2;

/// `JVMTI_ERROR_CLASS_NOT_PREPARED` for a class that is neither prepared
/// nor an array, as HotSpot answers `GetClassFields` and
/// `GetImplementedInterfaces` (measured on 25.0.3: a nested class named only
/// by its class literal answers 22, L1W52JvmtiThreadAndClassFacts); a
/// primitive class is never refused.
pub(in crate::jvmti::native_env) fn prepared(class: &crate::classloading::Class) -> Result<(), JInt> {
    let status = class_status(class);
    if status & (CLASS_STATUS_PREPARED | CLASS_STATUS_ARRAY | CLASS_STATUS_PRIMITIVE) == 0 {
        return Err(ERR_CLASS_NOT_PREPARED);
    }
    Ok(())
}

/// Slot 53: `GetClassFields(env, klass, field_count_ptr, fields_ptr)`: the
/// fields `klass` declares, in class-file order (none for an array or a
/// primitive class); `JVMTI_ERROR_CLASS_NOT_PREPARED` before the class is
/// prepared.
pub(in crate::jvmti::native_env) extern "C" fn get_class_fields(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field_count_ptr: *mut JInt,
    fields_ptr: *mut *mut u64,
) -> JInt {
    let ids = with_jvmti_class(
        env,
        klass,
        |_, class| prepared(class).map(|()| declared_field_ids(class)),
        // The i54-L1 retirement: a lambda proxy class's `arg$N` fields
        // ([`proxy_field`]).
        |shared, id| {
            let count = lambda_proxy_facts(shared, id).map_or(0, |facts| facts.fields.len());
            let class_half = u64::from(id.as_u32()) << 32;
            // Widening: a capture index into the id's low half.
            Ok((0..count).map(|i| class_half | i as u64).collect())
        },
        // Wave 56: a primitive class declares none.
        |_| Ok(Vec::new()),
    );
    match ids {
        Ok(Ok(ids)) => write_id_array(field_count_ptr, fields_ptr, &ids),
        Ok(Err(code)) | Err(code) => code,
    }
}

/// Slot 54: `GetImplementedInterfaces(env, klass, interface_count_ptr,
/// interfaces_ptr)`: the direct superinterfaces, in declaration order (none
/// for an array or a primitive class); `JVMTI_ERROR_CLASS_NOT_PREPARED`
/// before the class is prepared.
pub(in crate::jvmti::native_env) extern "C" fn get_implemented_interfaces(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    interface_count_ptr: *mut JInt,
    interfaces_ptr: *mut *mut JObject,
) -> JInt {
    let interfaces = with_jvmti_class(
        env,
        klass,
        |_, class| {
            prepared(class)?;
            Ok(if class.name.starts_with('[') {
                Vec::new()
            } else {
                class
                    .interfaces
                    .iter()
                    .map(|&id| jni::class_id_to_jclass(id))
                    .collect::<Vec<JObject>>()
            })
        },
        // Wave 55: a lambda proxy class (always prepared).
        |shared, id| {
            Ok(lambda_proxy_facts(shared, id)
                .map(|facts| facts.interfaces)
                .unwrap_or_default()
                .into_iter()
                .map(jni::class_id_to_jclass)
                .collect())
        },
        // Wave 56: a primitive class has none.
        |_| Ok(Vec::new()),
    );
    match interfaces {
        Ok(Ok(ids)) => write_id_array(interface_count_ptr, interfaces_ptr, &ids),
        Ok(Err(code)) | Err(code) => code,
    }
}

/// Slot 78: `GetLoadedClasses(env, class_count_ptr, classes_ptr)`
/// (interpreter round i2 wave 53, lane L1): every class of the VM, array
/// classes included and the primitive classes not, as HotSpot lists the
/// class-loader data graph; a class still being defined (`Loading`) is not
/// loaded yet. Live phase only.
pub(in crate::jvmti::native_env) extern "C" fn get_loaded_classes(
    env: *mut JvmtiNativeEnv,
    class_count_ptr: *mut JInt,
    classes_ptr: *mut *mut JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if class_count_ptr.is_null() || classes_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    const PRIMITIVES: [&str; 9] = [
        "boolean", "byte", "char", "short", "int", "long", "float", "double", "void",
    ];
    let mut classes: Vec<JObject> = shared
        .classes
        .class_manager
        .read()
        .class_store
        // zt t18/df7 (x13 item 21a): not an unbound or withdrawn define.
        .iter_named()
        .filter(|class| {
            !matches!(class.state, crate::classloading::ClassState::Loading)
                && !PRIMITIVES.contains(&&*class.name)
        })
        .map(|class| jni::class_id_to_jclass(class.id))
        .collect();
    // Wave 55: and the lambda proxy classes, which HotSpot lists as the
    // hidden classes they are (`Instrumentation.getAllLoadedClasses` lists
    // them too, `instrument::native_get_all_loaded_classes0`), while
    // `DebuggerGates::c_lambda_classes` is on.
    if shared.debug.debugger_gates.c_lambda_classes() {
        classes.extend(
            shared
                .classes
                .lambda_proxies
                .read()
                .keys()
                .map(|&id| jni::class_id_to_jclass(id)),
        );
    }
    write_id_array(class_count_ptr, classes_ptr, &classes)
}

/// Slot 79: `GetClassLoaderClasses(env, initiating_loader, class_count_ptr,
/// classes_ptr)` (interpreter round i2 wave 54, lane L1): the classes
/// `initiating_loader` (NULL: the bootstrap loader) is an initiating loader
/// of -- libinstrument's `getInitiatedClasses0` is this function, so the list
/// is `Instrumentation.getInitiatedClasses`'
/// (`instrument::initiated_class_ids`, measured against HotSpot by lane L3
/// in waves 41-42). Live phase only; the loader is decoded on the calling
/// thread, which must be attached.
pub(in crate::jvmti::native_env) extern "C" fn get_class_loader_classes(
    env: *mut JvmtiNativeEnv,
    initiating_loader: JObject,
    class_count_ptr: *mut JInt,
    classes_ptr: *mut *mut JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if class_count_ptr.is_null() || classes_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let ids = on_calling_thread(&shared, |ctx| {
        let loader = match jni::jobject_to_obj(initiating_loader) {
            Some(loader) => Some(loader),
            None if initiating_loader != 0 => return Err(ERR_INVALID_OBJECT),
            None => None,
        };
        Ok(crate::runtime::instrument::initiated_class_ids(ctx, loader))
    });
    match ids {
        Ok(Ok(ids)) => {
            let classes: Vec<JObject> = ids.into_iter().map(jni::class_id_to_jclass).collect();
            write_id_array(class_count_ptr, classes_ptr, &classes)
        }
        Ok(Err(code)) | Err(code) => code,
    }
}

// --- One decode of a `jclass` (interpreter round i2 wave 57, lane L1) ------
//
// Proposal
// `docs/feature-designs/jvmti-delivery-threading.md`.
// A class function decodes its handle once, inside one foreign-thread entry,
// into the kind of class it names ([`jvmti_class`]), and answers per kind
// ([`with_jvmti_class`]); waves 54-56 decoded it with `class_of` and, on
// `JVMTI_ERROR_INVALID_CLASS`, once more per fallback (a lambda proxy, a
// primitive class's mirror). The stored-class path is first and costs what
// `class_of` cost.

/// What a `jclass` names to this table, before the gates
/// ([`decode_class`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) enum DecodedClass {
    /// A class the class store holds ([`class_of`]'s answer).
    Stored(ClassId),
    /// A lambda proxy class the VM minted outside the store
    /// (`ClassRealm::lambda_proxies`).
    LambdaProxy(ClassId),
    /// A primitive class's mirror (`int.class`), by its Java name.
    Primitive(String),
    /// Some other `java.lang.Class` mirror with no `ClassId`.
    OtherMirror,
}

/// What a `jclass` names to the class functions: [`DecodedClass`] after
/// the gates (`DebuggerGates::c_lambda_classes`,
/// `DebuggerGates::c_primitive_classes`; a gated-off kind is
/// `JVMTI_ERROR_INVALID_CLASS`, as before the kind was known).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::jvmti::native_env) enum JvmtiClass {
    Stored(ClassId),
    LambdaProxy(ClassId),
    Primitive(String),
}

/// The kind of class `klass` names in `shared`, ungated; `None` for NULL
/// and for a handle that names no class (a `String`, a stale id). One
/// foreign-thread entry; a stored class answers with no further lock than
/// [`class_of`]'s.
pub(in crate::jvmti::native_env) fn decode_class(shared: &SharedVm, klass: JObject) -> Option<DecodedClass> {
    if klass == 0 {
        return None;
    }
    let fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    // A mirror with no `ClassId` is read inside the entry (the object may
    // move once it is left), as wave 54's `is_unlisted_class` read it.
    let decoded = match jni::jclass_class_id_exact(klass) {
        Some(id) => Ok(id),
        None => Err(jni::jobject_to_obj(klass).and_then(|obj| decode_mirror(shared, obj))),
    };
    jni::restore_jni_context(prev);
    drop(fx);
    match decoded {
        Ok(id) => {
            if shared.classes.class_manager.read().get_class(id).is_some() {
                Some(DecodedClass::Stored(id))
            } else if is_lambda_proxy(shared, id) {
                Some(DecodedClass::LambdaProxy(id))
            } else {
                None
            }
        }
        Err(mirror) => mirror,
    }
}

/// [`decode_class`] of a handle with no `ClassId`: a primitive class's
/// mirror, another `java.lang.Class` mirror, or no class.
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn decode_mirror(shared: &SharedVm, obj: ObjectRef) -> Option<DecodedClass> {
    let primitive = shared
        .classes
        .primitive_mirrors
        .read()
        .iter()
        .find(|(_, mirror)| mirror.as_ptr() == obj.as_ptr())
        .map(|(name, _)| name.clone());
    if let Some(name) = primitive {
        return Some(DecodedClass::Primitive(name));
    }
    let cid = shared.mem.heap.class_id_of(obj);
    let is_mirror = shared
        .classes
        .class_manager
        .read()
        .get_class(cid)
        .is_some_and(|class| &*class.name == "java/lang/Class");
    is_mirror.then_some(DecodedClass::OtherMirror)
}

/// [`decode_class`] under the gates: the class a class function answers
/// for, or `JVMTI_ERROR_INVALID_CLASS`.
pub(in crate::jvmti::native_env) fn jvmti_class(shared: &SharedVm, klass: JObject) -> Result<JvmtiClass, JInt> {
    match decode_class(shared, klass) {
        Some(DecodedClass::Stored(id)) => Ok(JvmtiClass::Stored(id)),
        Some(DecodedClass::LambdaProxy(id)) if shared.debug.debugger_gates.c_lambda_classes() => {
            Ok(JvmtiClass::LambdaProxy(id))
        }
        Some(DecodedClass::Primitive(name))
            if shared.debug.debugger_gates.c_primitive_classes() =>
        {
            Ok(JvmtiClass::Primitive(name))
        }
        _ => Err(ERR_INVALID_CLASS),
    }
}

/// A class function's answer for `klass` by its kind ([`jvmti_class`]):
/// `stored` reads a stored class under the class-manager read lock, `proxy`
/// answers a lambda proxy class, `primitive` a primitive class by its Java
/// name. Live or start phase (wave 52), the VM must exist.
pub(in crate::jvmti::native_env) fn with_jvmti_class<R>(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    stored: impl FnOnce(&SharedVm, &crate::classloading::Class) -> R,
    proxy: impl FnOnce(&SharedVm, ClassId) -> R,
    primitive: impl FnOnce(&str) -> R,
) -> Result<R, JInt> {
    let env = live_env(env)?;
    // Wave 94 (i54 stage 2): every caller is a `phase="start"` function.
    let shared = env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    match jvmti_class(&shared, klass)? {
        JvmtiClass::Stored(class_id) => {
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(class_id).ok_or(ERR_INVALID_CLASS)?;
            Ok(stored(&shared, class))
        }
        JvmtiClass::LambdaProxy(id) => Ok(proxy(&shared, id)),
        JvmtiClass::Primitive(name) => Ok(primitive(&name)),
    }
}

// --- Lambda proxy classes (interpreter round i2 wave 55, lane L1) ----------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// The VM mints a lambda's class outside the class store
// (`ClassRealm::lambda_proxies`), so [`class_of`] answers `None` for one; the
// class functions answer it as [`JvmtiClass::LambdaProxy`] (wave 57,
// [`with_jvmti_class`]; waves 55-56 on `class_of`'s failure path), with
// HotSpot's facts of the hidden class `InnerClassLambdaMetafactory` spins
// (measured, `L1W55JvmtiLambdaClass`).

/// `ACC_SYNTHETIC | ACC_FINAL | ACC_SUPER`, a lambda class's modifiers
/// (measured `0x1030`).
pub(in crate::jvmti::native_env) const LAMBDA_PROXY_MODIFIERS: JInt =
    crate::runtime::lambda_proxy::LAMBDA_PROXY_CLASS_MODIFIERS as JInt;
/// `VERIFIED | PREPARED | INITIALIZED`: a lambda class exists only once its
/// call site was linked (measured `7`).
pub(in crate::jvmti::native_env) const LAMBDA_PROXY_STATUS: JInt = 7;
/// The class-file major version of a spun lambda class: the running JDK's
/// latest (`ClassFileFormatVersion.latest()`, 69 for JDK 25; measured).
pub(in crate::jvmti::native_env) const LAMBDA_PROXY_MAJOR_VERSION: JInt = 69;

/// Is `id` one of `shared`'s lambda proxy classes?
pub(in crate::jvmti::native_env) fn is_lambda_proxy(shared: &SharedVm, id: ClassId) -> bool {
    shared.classes.lambda_proxies.read().contains_key(&id)
}

// --- Primitive classes (interpreter round i2 wave 56, lane L1) -------------
//
// A primitive class's mirror (`int.class`) has no `ClassId`, so
// [`class_of`] answers `None` for one and every class function refused it
// with `JVMTI_ERROR_INVALID_CLASS`. Behind `CRATONVM_JVMTI_C_PRIMITIVE_CLASSES`
// (`DebuggerGates::c_primitive_classes`; [`JvmtiClass::Primitive`] since
// wave 57) they answer HotSpot's facts
// (measured, `L1W56JvmtiPrimitiveClass`): the type's letter, status
// `PRIMITIVE`, `0x411`, no methods, fields or interfaces, neither an
// interface nor an array, the bootstrap loader, no source file and no
// class-file version (101).

/// `public final abstract`, a primitive class's modifiers (measured `0x411`).
pub(in crate::jvmti::native_env) const PRIMITIVE_CLASS_MODIFIERS: JInt = 0x0411;

/// The facts of lambda proxy `id` (wave 56: one record,
/// `runtime::lambda_proxy::lambda_proxy_facts`, where wave 55 re-derived
/// each one here): its host, signature, interfaces and methods.
pub(in crate::jvmti::native_env) fn lambda_proxy_facts(shared: &SharedVm, id: ClassId) -> Option<LambdaProxyFacts> {
    crate::runtime::lambda_proxy::lambda_proxy_facts(shared, id)
}

/// `GetClassSignature` of lambda proxy `id`: HotSpot's hidden-class
/// signature ([`LambdaProxyFacts::signature`]).
pub(in crate::jvmti::native_env) fn lambda_proxy_signature(shared: &SharedVm, id: ClassId) -> Option<String> {
    lambda_proxy_facts(shared, id).map(|facts| facts.signature(id))
}

// --- A lambda proxy's methods (interpreter round i2 wave 56, lane L1) -----
//
// Behind `CRATONVM_JVMTI_C_LAMBDA_METHODS` (`DebuggerGates::c_lambda_methods`).
// A proxy has no `Class` in the store, so its `jmethodID`s are its id and the
// method's index in [`LambdaProxyFacts::methods`] (`(proxy id << 32) |
// index`, as a real class's); the method functions answer one on their
// `JVMTI_ERROR_INVALID_METHODID` path only ([`proxy_method`], cold), with the
// facts HotSpot answers for the spun class's methods (measured,
// `L1W56JvmtiLambdaMethods`): not native, not synthetic, no generic
// signature, no line numbers. The spun code itself is not modelled:
// `GetMethodLocation` and `GetBytecodes` answer
// `JVMTI_ERROR_ABSENT_INFORMATION` where HotSpot answers the code's range and
// bytes (open on the lambda page).

/// The proxy method a `jmethodID` names, with its declaring proxy class;
/// `None` while `DebuggerGates::c_lambda_methods` is off, and for any id that
/// names no proxy method.
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn proxy_method(shared: &SharedVm, method: u64) -> Option<(ClassId, LambdaProxyMethod)> {
    if !shared.debug.debugger_gates.c_lambda_methods() {
        return None;
    }
    // Cast: the id's halves, as `jni::decode_method_id` reads them.
    let class_id = ClassId::new((method >> 32) as u32);
    // Cast: the low 32 bits; a proxy declares a handful of methods.
    let index = (method & 0xFFFF_FFFF) as usize;
    if !is_lambda_proxy(shared, class_id) {
        return None;
    }
    let facts = lambda_proxy_facts(shared, class_id)?;
    let m = facts.methods.into_iter().nth(index)?;
    Some((class_id, m))
}

/// `GetClassMethods`' ids of lambda proxy class `id`, in the spun class
/// file's order; `None` while `DebuggerGates::c_lambda_methods` is off or
/// the proxy's facts are not known.
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn lambda_proxy_method_ids(shared: &SharedVm, id: ClassId) -> Option<Vec<u64>> {
    if !shared.debug.debugger_gates.c_lambda_methods() {
        return None;
    }
    let count = lambda_proxy_facts(shared, id)?.methods.len();
    let ids = (0..count)
        .filter_map(|index| u32::try_from(index).ok())
        .map(|index| (u64::from(id.as_u32()) << 32) | u64::from(index))
        .collect();
    // Positive control: `CRATONVM_FRAME_TRACE=1`, as the other-thread rows.
    if crate::runtime::env_cache::frame_trace() {
        eprintln!(
            "[JVMTI_C_LAMBDA_METHODS] class={:#x} methods={count}",
            id.as_u32()
        );
    }
    Some(ids)
}

/// `answer`, a method function's answer read from the class store; when that
/// is `None` (no such method) and `method` names a lambda proxy's method,
/// `proxy`'s answer for it instead.
pub(in crate::jvmti::native_env) fn or_proxy_method<R>(
    shared: &SharedVm,
    method: u64,
    answer: Option<R>,
    proxy: impl FnOnce(ClassId, &LambdaProxyMethod) -> R,
) -> Option<R> {
    match answer {
        Some(answer) => Some(answer),
        None => proxy_method(shared, method).map(|(class_id, m)| proxy(class_id, &m)),
    }
}

/// Slot 45: `IsModifiableClass(env, klass, is_modifiable_class_ptr)`
/// (interpreter round i2 wave 54, lane L1): `JNI_FALSE` for a primitive,
/// array or hidden class, as `Instrumentation.isModifiableClass` answers
/// (`instrument::class_id_is_modifiable`); no capability, as on HotSpot.
/// Start or live phase.
pub(in crate::jvmti::native_env) extern "C" fn is_modifiable_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    is_modifiable_class_ptr: *mut u8,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.start_or_live_vm() else {
        return ERR_WRONG_PHASE;
    };
    // A class the store does not hold but HotSpot knows (a primitive class,
    // a lambda proxy, whatever the gates say) is not modifiable (wave 54).
    let class_id = match decode_class(&shared, klass) {
        Some(DecodedClass::Stored(class_id)) => class_id,
        Some(_) => return write_jboolean(is_modifiable_class_ptr, Ok(false)),
        None => return ERR_INVALID_CLASS,
    };
    let answer = on_calling_thread(&shared, |ctx| {
        crate::runtime::instrument::class_id_is_modifiable(&*ctx, class_id)
    });
    write_jboolean(is_modifiable_class_ptr, answer)
}

/// The `jvmtiError` of a refused redefinition, from libinstrument's
/// throwable for it (`RedefinitionRefusal::instrument_throwable`, the
/// inverse of `JPLISAgent`'s mapping of these errors). Shared with the JDWP
/// server's `VirtualMachine.RedefineClasses` since interpreter round i5 wave
/// 104 (lane L1; `debug::redefine_classes`).
#[cold]
pub(crate) fn redefinition_refusal_error(refusal: &cratonvm_native_api::InstrumentRefusal) -> JInt {
    let message = refusal.message.unwrap_or_default();
    match refusal.throwable {
        "java/lang/ClassFormatError" => 60,
        "java/lang/VerifyError" => 62,
        "java/lang/NoClassDefFoundError" => 69,
        "java/lang/UnsupportedClassVersionError" => 68,
        "java/lang/ClassCircularityError" => 61,
        "java/lang/instrument/UnmodifiableClassException" => ERR_UNMODIFIABLE_CLASS,
        "java/lang/UnsupportedOperationException" => {
            if message.contains("add a method") {
                63
            } else if message.contains("schema") {
                64
            } else if message.contains("superclass or interfaces") {
                66
            } else if message.contains("delete a method") {
                67
            } else if message.contains("class modifiers") {
                70
            } else if message.contains("method modifiers") {
                71
            } else if message.contains("NestHost") {
                72
            } else {
                73
            }
        }
        _ => ERR_INTERNAL,
    }
}

/// `jvmtiClassDefinition`: `{ jclass klass; jint class_byte_count; const
/// unsigned char* class_bytes; }`, 24 bytes with its padding.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(in crate::jvmti::native_env) struct ClassDefinition {
    pub(in crate::jvmti::native_env) klass: JObject,
    pub(in crate::jvmti::native_env) class_byte_count: JInt,
    pub(in crate::jvmti::native_env) class_bytes: *const u8,
}

/// Slot 87: `RedefineClasses(env, class_count, class_definitions)`
/// (interpreter round i2 wave 57, lane L1; `can_redefine_classes`, while
/// `DebuggerGates::c_redefine` is on): each new class file through the
/// whole `ClassFileLoadHook` chain (the C envs and the Java transformers, as
/// a redefinition offers every one of them the bytes), then every class
/// checked and redefined at once (`instrument::redefine_classes_for_jvmti`,
/// `Instrumentation.redefineClasses`' path), on the calling thread, which
/// must be attached. HotSpot's checks, in its order (MEASURED,
/// `L1W57JvmtiRedefine`): the live phase, the capability, a negative count
/// (`JVMTI_ERROR_ILLEGAL_ARGUMENT`), a NULL array (`JVMTI_ERROR_NULL_POINTER`),
/// then per definition an invalid class (`JVMTI_ERROR_INVALID_CLASS`), a NULL
/// class file (`JVMTI_ERROR_NULL_POINTER`) and a primitive, array or hidden
/// class (`JVMTI_ERROR_UNMODIFIABLE_CLASS`), before any hook runs; a refused
/// redefinition answers its error ([`redefinition_refusal_error`]).
pub(in crate::jvmti::native_env) extern "C" fn redefine_classes(
    env: *mut JvmtiNativeEnv,
    class_count: JInt,
    class_definitions: *const ClassDefinition,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if !env.possesses(CAN_REDEFINE_CLASSES) {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let Ok(count) = usize::try_from(class_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if class_definitions.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's array of `class_count` definitions.
    let definitions = unsafe { std::slice::from_raw_parts(class_definitions, count) };
    let mut classes: Vec<(ClassId, Vec<u8>)> = Vec::with_capacity(count);
    for definition in definitions {
        let id = match decode_class(&shared, definition.klass) {
            Some(DecodedClass::Stored(id)) => Some(id),
            Some(_) => None,
            None => return ERR_INVALID_CLASS,
        };
        if definition.class_bytes.is_null() {
            return ERR_NULL_POINTER;
        }
        let Some(id) = id else {
            return ERR_UNMODIFIABLE_CLASS;
        };
        let len = usize::try_from(definition.class_byte_count).unwrap_or(0);
        // SAFETY: the agent's class file of `class_byte_count` bytes.
        let bytes = unsafe { std::slice::from_raw_parts(definition.class_bytes, len) }.to_vec();
        classes.push((id, bytes));
    }
    let outcome = on_calling_thread(&shared, |ctx| {
        if classes
            .iter()
            .any(|&(id, _)| !crate::runtime::instrument::class_id_is_modifiable(&*ctx, id))
        {
            return ERR_UNMODIFIABLE_CLASS;
        }
        match crate::runtime::instrument::redefine_classes_for_jvmti(ctx, &classes) {
            None => ERR_NONE,
            Some(refusal) => {
                tracing::warn!("RedefineClasses: {}", refusal.detail);
                redefinition_refusal_error(&refusal)
            }
        }
    });
    match outcome {
        Ok(code) | Err(code) => code,
    }
}

/// Slot 152: `RetransformClasses(env, class_count, classes)` (interpreter
/// round i2 wave 54, lane L1; `can_retransform_classes`, while
/// `DebuggerGates::c_retransform` is on): each class's retransformation base
/// through the retransform-capable `ClassFileLoadHook`s and Java transformers,
/// then every class redefined at once (`instrument::retransform_classes_for_jvmti`),
/// on the calling thread, which must be attached. HotSpot's checks, in its
/// order: the live phase, the capability, a negative count
/// (`JVMTI_ERROR_ILLEGAL_ARGUMENT`), a NULL array (`JVMTI_ERROR_NULL_POINTER`),
/// then per class an invalid one (`JVMTI_ERROR_INVALID_CLASS`) and a
/// primitive, array or hidden one (`JVMTI_ERROR_UNMODIFIABLE_CLASS`), before
/// any hook runs; a refused redefinition answers its error.
pub(in crate::jvmti::native_env) extern "C" fn retransform_classes(
    env: *mut JvmtiNativeEnv,
    class_count: JInt,
    classes: *const JObject,
) -> JInt {
    let env = match live_env(env) {
        Ok(env) => env,
        Err(code) => return code,
    };
    let Some(shared) = env.vm() else {
        return ERR_WRONG_PHASE;
    };
    if shared.debug.debugger_gates.jvmti_outside_live() {
        return ERR_WRONG_PHASE;
    }
    if !env.can_retransform() {
        return ERR_MUST_POSSESS_CAPABILITY;
    }
    let Ok(count) = usize::try_from(class_count) else {
        return ERR_ILLEGAL_ARGUMENT;
    };
    if classes.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's array of `class_count` `jclass`es.
    let handles = unsafe { std::slice::from_raw_parts(classes, count) };
    let mut ids = Vec::with_capacity(count);
    for &klass in handles {
        match decode_class(&shared, klass) {
            Some(DecodedClass::Stored(id)) => ids.push(id),
            Some(_) => return ERR_UNMODIFIABLE_CLASS,
            None => return ERR_INVALID_CLASS,
        }
    }
    let outcome = on_calling_thread(&shared, |ctx| {
        if ids
            .iter()
            .any(|&id| !crate::runtime::instrument::class_id_is_modifiable(&*ctx, id))
        {
            return ERR_UNMODIFIABLE_CLASS;
        }
        match crate::runtime::instrument::retransform_classes_for_jvmti(ctx, &ids) {
            None => ERR_NONE,
            Some(refusal) => {
                tracing::warn!("RetransformClasses: {}", refusal.detail);
                redefinition_refusal_error(&refusal)
            }
        }
    });
    match outcome {
        Ok(code) | Err(code) => code,
    }
}

/// Slot 55: `IsInterface(env, klass, is_interface_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn is_interface(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    is_interface_ptr: *mut u8,
) -> JInt {
    // Wave 55: a lambda proxy class is neither an interface nor an array.
    // Wave 56: so is a primitive class.
    let answer = with_jvmti_class(
        env,
        klass,
        |_, class| class.access_flags.bits() & ACC_INTERFACE != 0,
        |_, _| false,
        |_| false,
    );
    write_jboolean(is_interface_ptr, answer)
}

/// Slot 56: `IsArrayClass(env, klass, is_array_class_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn is_array_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    is_array_class_ptr: *mut u8,
) -> JInt {
    let answer = with_jvmti_class(
        env,
        klass,
        |_, class| class.name.starts_with('['),
        |_, _| false,
        |_| false,
    );
    write_jboolean(is_array_class_ptr, answer)
}

/// Slot 57: `GetClassLoader(env, klass, classloader_ptr)`: the class's
/// defining loader as the program sees it (`Class.getClassLoader0`,
/// `debug::inspect::class_loader_of`), NULL for the bootstrap loader, as a
/// local reference. Asked on the calling thread, which must be attached.
pub(in crate::jvmti::native_env) extern "C" fn get_class_loader(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    classloader_ptr: *mut JObject,
) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let class_id = match jvmti_class(&shared, klass) {
        Ok(JvmtiClass::Stored(class_id)) => Some(class_id),
        // Wave 55: a lambda proxy class is its host's loader's (its defining
        // loader, as the hidden class's lookup class's); NULL when the host
        // is not known.
        Ok(JvmtiClass::LambdaProxy(proxy)) => {
            lambda_proxy_facts(&shared, proxy).and_then(|facts| facts.host_id)
        }
        // Wave 56: a primitive class is the bootstrap loader's (NULL).
        Ok(JvmtiClass::Primitive(_)) => None,
        Err(code) => return code,
    };
    if classloader_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let loader = on_calling_thread(&shared, |ctx| {
        class_id
            .and_then(|class_id| {
                crate::debug::inspect::class_loader_of(&shared, ctx.thread, class_id)
            })
            .map_or(0, jni::new_local_handle)
    });
    match loader {
        Ok(loader) => {
            // SAFETY: the agent's non-null out-parameter.
            unsafe { *classloader_ptr = loader };
            ERR_NONE
        }
        Err(code) => code,
    }
}

/// Slot 145: `GetClassVersionNumbers(env, klass, minor_version_ptr,
/// major_version_ptr)`: `JVMTI_ERROR_ABSENT_INFORMATION` for an array or a
/// primitive class.
pub(in crate::jvmti::native_env) extern "C" fn get_class_version_numbers(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    minor_version_ptr: *mut JInt,
    major_version_ptr: *mut JInt,
) -> JInt {
    let version = with_jvmti_class(
        env,
        klass,
        |_, class| {
            (!class.name.starts_with('[') && !is_primitive_class_name(&class.name)).then(|| {
                (
                    JInt::from(class.version.minor),
                    JInt::from(class.version.major),
                )
            })
        },
        // Wave 55: a lambda proxy class.
        |_, _| Some((0, LAMBDA_PROXY_MAJOR_VERSION)),
        // Wave 56: a primitive class has no class file (measured 101).
        |_| None,
    );
    let version = match version {
        Ok(version) => version,
        Err(code) => return code,
    };
    if minor_version_ptr.is_null() || major_version_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some((minor, major)) = version else {
        return ERR_ABSENT_INFORMATION;
    };
    // SAFETY: the agent's non-null out-parameters.
    unsafe {
        *minor_version_ptr = minor;
        *major_version_ptr = major;
    }
    ERR_NONE
}

// --- Fields (slots 60-63) ---------------------------------------------------

/// `read` of the field a `jfieldID` names in `klass` (its declaring class),
/// or the error to answer. An id with [`STATIC_FIELD_ID`] names a static
/// field; one without it names the instance field of that slot, else (an
/// unmarked id `FromReflectedField` made of a static field) the static field
/// of that index.
pub(in crate::jvmti::native_env) fn with_field<R>(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    read: impl FnOnce(&crate::classloading::Class, &cratonvm_reader::field::ClassFileField) -> R,
) -> Result<R, JInt> {
    let env = live_env(env)?;
    // Wave 94 (i54 stage 2): the field facts are `phase="start"`, the field
    // watches (`events_more::watched_field`) live-only.
    let shared = env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)?;
    let class_id = class_of(&shared, klass).ok_or(ERR_INVALID_CLASS)?;
    // Cast: the id's class half, without the static mark.
    let declaring = ClassId::new(((field & !STATIC_FIELD_ID) >> 32) as u32);
    if field == 0 || declaring != class_id {
        return Err(ERR_INVALID_FIELDID);
    }
    let index = usize::try_from(field & 0xFFFF_FFFF).map_err(|_| ERR_INVALID_FIELDID)?;
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id).ok_or(ERR_INVALID_CLASS)?;
    let nth_static = || class.fields.iter().filter(|f| f.is_static()).nth(index);
    let found = if field & STATIC_FIELD_ID != 0 {
        nth_static()
    } else {
        class.field_at_index(index).or_else(nth_static)
    };
    let found = found.ok_or(ERR_INVALID_FIELDID)?;
    Ok(read(class, found))
}

/// `(proxy id, name, descriptor)` of capture field `field` of `klass` when
/// `klass` is a lambda proxy class (the i54-L1 retirement): its `arg$N`
/// fields' ids are `(proxy id << 32) | capture index` -- the capture's field
/// slot in the proxy object, which JNI's `Get<Type>Field` reads
/// (`jni::decode_field_id`). `None` for any other class. Cold: asked on
/// [`with_field`]'s error path only.
#[cold]
#[inline(never)]
pub(in crate::jvmti::native_env) fn proxy_field(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
) -> Option<Result<(ClassId, String, String), JInt>> {
    let shared = live_env(env).ok()?.vm()?;
    let JvmtiClass::LambdaProxy(id) = jvmti_class(&shared, klass).ok()? else {
        return None;
    };
    // Cast: the id's class half, whole: a proxy's id has bit 31 set
    // (`LAMBDA_PROXY_ID_BASE`), so its field ids carry the bit that marks a
    // stored class's static field ([`STATIC_FIELD_ID`]); a proxy declares none.
    if (field >> 32) as u32 != id.as_u32() {
        return Some(Err(ERR_INVALID_FIELDID));
    }
    let Ok(index) = usize::try_from(field & 0xFFFF_FFFF) else {
        return Some(Err(ERR_INVALID_FIELDID));
    };
    let Some(facts) = lambda_proxy_facts(&shared, id) else {
        return Some(Err(ERR_INVALID_CLASS));
    };
    Some(
        facts
            .fields
            .into_iter()
            .nth(index)
            .map(|(name, descriptor)| (id, name, descriptor))
            .ok_or(ERR_INVALID_FIELDID),
    )
}

/// `answer`, or on its `JVMTI_ERROR_INVALID_CLASS` or `INVALID_FIELDID` (the
/// proxy's id names no stored class) the answer `proxy` gives for a lambda
/// proxy's capture field ([`proxy_field`]).
pub(in crate::jvmti::native_env) fn or_proxy_field<R>(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    answer: Result<R, JInt>,
    proxy: impl FnOnce(ClassId, String, String) -> R,
) -> Result<R, JInt> {
    match answer {
        Err(code @ (ERR_INVALID_CLASS | ERR_INVALID_FIELDID)) => {
            match proxy_field(env, klass, field) {
                Some(found) => found.map(|(id, name, descriptor)| proxy(id, name, descriptor)),
                None => Err(code),
            }
        }
        other => other,
    }
}

/// Slot 60: `GetFieldName(env, klass, field, name_ptr, signature_ptr,
/// generic_ptr)`: any out-parameter may be NULL; the generic signature is
/// the field's `Signature` attribute, NULL without one.
pub(in crate::jvmti::native_env) extern "C" fn get_field_name(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    name_ptr: *mut *mut u8,
    signature_ptr: *mut *mut u8,
    generic_ptr: *mut *mut u8,
) -> JInt {
    let names = with_field(env, klass, field, |_, f| {
        (
            f.name.to_string(),
            f.descriptor.to_string(),
            crate::debug::signature_attribute(&f.attributes),
        )
    });
    let names = or_proxy_field(env, klass, field, names, |_, name, descriptor| {
        (name, descriptor, None)
    });
    let (name, signature, generic) = match names {
        Ok(names) => names,
        Err(code) => return code,
    };
    let out = [
        (name_ptr, Some(name.as_str())),
        (signature_ptr, Some(signature.as_str())),
        (generic_ptr, generic.as_deref()),
    ];
    if write_modified_utf8_outs(&out) {
        ERR_NONE
    } else {
        ERR_OUT_OF_MEMORY
    }
}

/// Slot 61: `GetFieldDeclaringClass(env, klass, field, declaring_class_ptr)`.
pub(in crate::jvmti::native_env) extern "C" fn get_field_declaring_class(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    declaring_class_ptr: *mut JObject,
) -> JInt {
    let declaring = with_field(env, klass, field, |class, _| class.id);
    let declaring = match or_proxy_field(env, klass, field, declaring, |id, _, _| id) {
        Ok(id) => id,
        Err(code) => return code,
    };
    if declaring_class_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *declaring_class_ptr = jni::class_id_to_jclass(declaring) };
    ERR_NONE
}

/// Slot 62: `GetFieldModifiers(env, klass, field, modifiers_ptr)`: the
/// field's access flags that JVMTI recognises
/// (`JVM_RECOGNIZED_FIELD_MODIFIERS`, synthetic and enum included).
pub(in crate::jvmti::native_env) extern "C" fn get_field_modifiers(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    modifiers_ptr: *mut JInt,
) -> JInt {
    let modifiers = with_field(env, klass, field, |_, f| {
        JInt::from(f.access_flags.bits()) & crate::debug::JVM_RECOGNIZED_FIELD_MODIFIERS as JInt
    });
    let modifiers = or_proxy_field(env, klass, field, modifiers, |_, _, _| {
        JInt::from(crate::runtime::lambda_proxy::LAMBDA_PROXY_FIELD_MODIFIERS)
    });
    write_jint(modifiers_ptr, modifiers)
}

/// Slot 63: `IsFieldSynthetic(env, klass, field, is_synthetic_ptr)`
/// (`can_get_synthetic_attribute`): `ACC_SYNTHETIC` or a `Synthetic`
/// attribute.
pub(in crate::jvmti::native_env) extern "C" fn is_field_synthetic(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    is_synthetic_ptr: *mut u8,
) -> JInt {
    match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_SYNTHETIC_ATTRIBUTE) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(_) => {}
        Err(code) => return code,
    }
    let answer = with_field(env, klass, field, |_, f| {
        f.access_flags.bits() & ACC_SYNTHETIC != 0
            || crate::debug::has_synthetic_attribute(&f.attributes)
    });
    // A capture field is not synthetic (measured).
    let answer = or_proxy_field(env, klass, field, answer, |_, _, _| false);
    write_jboolean(is_synthetic_ptr, answer)
}

// --- Methods (slots 68, 69, 77) ---------------------------------------------

/// Slot 68: `GetMaxLocals(env, method, max_ptr)`: the `Code` attribute's
/// `max_locals`; `JVMTI_ERROR_NATIVE_METHOD` for a native method, 0 for an
/// abstract one.
pub(in crate::jvmti::native_env) extern "C" fn get_max_locals(env: *mut JvmtiNativeEnv, method: u64, max_ptr: *mut JInt) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let answer = with_method(&shared, method, |_, m| {
        if m.is_native() {
            Err(ERR_NATIVE_METHOD)
        } else {
            Ok(m.code().map_or(0, |code| JInt::from(code.max_locals)))
        }
    });
    // Wave 56: a lambda proxy's method (its code stores no local, measured).
    let answer = or_proxy_method(&shared, method, answer, |_, m| Ok(JInt::from(m.args_size)))
        .unwrap_or(Err(ERR_INVALID_METHODID));
    write_jint(max_ptr, answer)
}

/// Slot 69: `GetArgumentsSize(env, method, size_ptr)`: the local slots the
/// arguments take (`this` included, a `long` or a `double` two);
/// `JVMTI_ERROR_NATIVE_METHOD` for a native method.
pub(in crate::jvmti::native_env) extern "C" fn get_arguments_size(
    env: *mut JvmtiNativeEnv,
    method: u64,
    size_ptr: *mut JInt,
) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let answer = with_method(&shared, method, |_, m| {
        if m.is_native() {
            return Err(ERR_NATIVE_METHOD);
        }
        let slots = cratonvm_reader::method_descriptor::MethodDescriptor::parse(&m.descriptor)
            .map_err(|_| ERR_INVALID_METHODID)?
            .parameters_stack_slots();
        let this = usize::from(!m.is_static());
        JInt::try_from(slots + this).map_err(|_| ERR_INTERNAL)
    });
    // Wave 56: a lambda proxy's method.
    let answer = or_proxy_method(&shared, method, answer, |_, m| Ok(JInt::from(m.args_size)))
        .unwrap_or(Err(ERR_INVALID_METHODID));
    write_jint(size_ptr, answer)
}

/// Slot 77: `IsMethodSynthetic(env, method, is_synthetic_ptr)`
/// (`can_get_synthetic_attribute`).
pub(in crate::jvmti::native_env) extern "C" fn is_method_synthetic(
    env: *mut JvmtiNativeEnv,
    method: u64,
    is_synthetic_ptr: *mut u8,
) -> JInt {
    let shared = match live_env(env) {
        Ok(e) if !e.possesses(CAN_GET_SYNTHETIC_ATTRIBUTE) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(e) => match e.start_or_live_vm() {
            Some(shared) => shared,
            None => return ERR_WRONG_PHASE,        },
        Err(code) => return code,
    };
    let answer = with_method(&shared, method, |_, m| {
        m.access_flags.bits() & ACC_SYNTHETIC != 0
            || crate::debug::has_synthetic_attribute(&m.attributes)
    });
    // Wave 56: a lambda proxy's method (never `ACC_SYNTHETIC`, measured).
    let answer = or_proxy_method(&shared, method, answer, |_, m| {
        m.access_flags & ACC_SYNTHETIC != 0
    })
    .ok_or(ERR_INVALID_METHODID);
    write_jboolean(is_synthetic_ptr, answer)
}

// --- Thread CPU time, thread-group children, obsolete methods (interpreter
// round i2 wave 56, lane L1) ------------------------------------------------
//
// `docs/feature-designs/jvmti-delivery-threading.md`.
// Each answer HotSpot 25.0.3's, measured (`L1W56JvmtiThreadTimes`).

/// `can_get_current_thread_cpu_time`, the 23rd bit field, and
/// `can_get_thread_cpu_time`, the 24th: potential in the OnLoad and the live
/// phase (measured).
pub(in crate::jvmti::native_env) const CAN_GET_CURRENT_THREAD_CPU_TIME: u32 = 1 << 22;
pub(in crate::jvmti::native_env) const CAN_GET_THREAD_CPU_TIME: u32 = 1 << 23;

/// `jvmtiTimerInfo`: `{ jlong max_value; jboolean may_skip_forward;
/// jboolean may_skip_backward; jvmtiTimerKind kind; jlong reserved1; jlong
/// reserved2; }`, 32 bytes.
#[repr(C)]
pub(in crate::jvmti::native_env) struct TimerInfoRow {
    pub(in crate::jvmti::native_env) max_value: i64,
    pub(in crate::jvmti::native_env) may_skip_forward: u8,
    pub(in crate::jvmti::native_env) may_skip_backward: u8,
    pub(in crate::jvmti::native_env) kind: JInt,
    pub(in crate::jvmti::native_env) reserved1: i64,
    pub(in crate::jvmti::native_env) reserved2: i64,
}

/// `JVMTI_TIMER_TOTAL_CPU`: user and system time.
pub(in crate::jvmti::native_env) const TIMER_TOTAL_CPU: JInt = 31;

/// The env behind `env` with VM, when it holds `capability` (word 0):
/// `JVMTI_ERROR_MUST_POSSESS_CAPABILITY` without it (measured: an env
/// obtained in the live phase that did not add it gets 99),
/// `JVMTI_ERROR_WRONG_PHASE` before the VM is bound.
pub(in crate::jvmti::native_env) fn cpu_time_env(env: *mut JvmtiNativeEnv, capability: u32) -> Result<Arc<SharedVm>, JInt> {
    let env = live_env(env)?;
    if !env.possesses(capability) {
        return Err(ERR_MUST_POSSESS_CAPABILITY);
    }
    // Wave 94 (i54 stage 2): the current thread's CPU time is
    // `phase="start"`, another thread's live-only.
    env.start_or_live_vm().ok_or(ERR_WRONG_PHASE)
}

/// Slots 134 / 136: `GetCurrentThreadCpuTimerInfo` /
/// `GetThreadCpuTimerInfo(env, info_ptr)`: a total-CPU timer of 64 bits that
/// skips neither way (measured: `max=-1 forward=0 backward=0 kind=31`).
pub(in crate::jvmti::native_env) fn cpu_timer_info(env: *mut JvmtiNativeEnv, capability: u32, info_ptr: *mut TimerInfoRow) -> JInt {
    if let Err(code) = cpu_time_env(env, capability) {
        return code;
    }
    if info_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    // SAFETY: the agent's non-null out-parameter.
    unsafe {
        info_ptr.write(TimerInfoRow {
            max_value: -1,
            may_skip_forward: 0,
            may_skip_backward: 0,
            kind: TIMER_TOTAL_CPU,
            reserved1: 0,
            reserved2: 0,
        });
    }
    ERR_NONE
}

pub(in crate::jvmti::native_env) extern "C" fn get_current_thread_cpu_timer_info(
    env: *mut JvmtiNativeEnv,
    info_ptr: *mut TimerInfoRow,
) -> JInt {
    cpu_timer_info(env, CAN_GET_CURRENT_THREAD_CPU_TIME, info_ptr)
}

pub(in crate::jvmti::native_env) extern "C" fn get_thread_cpu_timer_info(
    env: *mut JvmtiNativeEnv,
    info_ptr: *mut TimerInfoRow,
) -> JInt {
    cpu_timer_info(env, CAN_GET_THREAD_CPU_TIME, info_ptr)
}

/// Does `thread` -- or, for NULL, the calling thread -- name a virtual
/// thread (interpreter round i2 wave 58, lane L1)? HotSpot answers the CPU
/// time functions `JVMTI_ERROR_UNSUPPORTED_OPERATION` for one (MEASURED,
/// `tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.java`). The handle is
/// read inside the caller's foreign-thread entry and `shared`'s JNI context,
/// as [`lookup_thread`] reads it; a handle that names no object is left to
/// the caller's own checks.
pub(in crate::jvmti::native_env) fn names_virtual_thread(shared: &SharedVm, thread: JObject) -> bool {
    if thread == 0 {
        return jni::current_jvm_thread_of(shared)
            // SAFETY: the calling thread's own `JvmThread`; `kind` is a
            // plain field it does not change while it runs this call.
            .is_some_and(|current| unsafe {
                matches!(
                    (*current).kind,
                    crate::threading::jvm_thread::ThreadKind::Virtual
                )
            });
    }
    let _fx = jni::ForeignJniEntry::enter();
    let prev = jni::replace_jni_context(shared);
    let obj = jni::jobject_to_obj(thread);
    jni::restore_jni_context(prev);
    obj.is_some_and(|obj| {
        let class_id = shared.mem.heap.class_id_of(obj);
        shared
            .classes
            .class_manager
            .read()
            .is_subclass_of_by_name(class_id, "java/lang/BaseVirtualThread")
    })
}

/// The calling thread's CPU time (user and system), in nanoseconds.
pub(in crate::jvmti::native_env) fn current_thread_cpu_nanos() -> Option<i64> {
    cratonvm_jit::tiered::current_thread_cpu_time()
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Slot 135: `GetCurrentThreadCpuTime(env, nanos_ptr)`
/// (`can_get_current_thread_cpu_time`).
pub(in crate::jvmti::native_env) extern "C" fn get_current_thread_cpu_time(env: *mut JvmtiNativeEnv, nanos_ptr: *mut i64) -> JInt {
    let shared = match cpu_time_env(env, CAN_GET_CURRENT_THREAD_CPU_TIME) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Wave 58 (lane L1): a virtual thread asking is `UNSUPPORTED_OPERATION`,
    // as on HotSpot (`L1W58JvmtiVirtualCpuTime`).
    if names_virtual_thread(&shared, 0) {
        return ERR_UNSUPPORTED_OPERATION;
    }
    if nanos_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let Some(nanos) = current_thread_cpu_nanos() else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *nanos_ptr = nanos };
    ERR_NONE
}

/// Slot 137: `GetThreadCpuTime(env, thread, nanos_ptr)`
/// (`can_get_thread_cpu_time`): a NULL `thread` or the calling thread reads
/// the calling thread's clock, another live thread its OS thread's
/// (`ThreadRegistry::os_tid_of`, read as `ThreadMXBean.getThreadCpuTime`
/// reads it); a thread not started or terminated is
/// `JVMTI_ERROR_THREAD_NOT_ALIVE`, an object that is no thread
/// `JVMTI_ERROR_INVALID_THREAD` (measured).
pub(in crate::jvmti::native_env) extern "C" fn get_thread_cpu_time(
    env: *mut JvmtiNativeEnv,
    thread: JObject,
    nanos_ptr: *mut i64,
) -> JInt {
    let shared = match cpu_time_env(env, CAN_GET_THREAD_CPU_TIME) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    // Wave 58 (lane L1): a virtual thread -- the handle's, or the calling
    // one for NULL -- is `UNSUPPORTED_OPERATION`, before the out-parameter
    // check, as HotSpot answers it (`L1W58JvmtiVirtualCpuTime`).
    if names_virtual_thread(&shared, thread) {
        return ERR_UNSUPPORTED_OPERATION;
    }
    if nanos_ptr.is_null() {
        return ERR_NULL_POINTER;
    }
    let current = jni::current_jvm_thread_of(&shared)
        // SAFETY: the calling thread's own `JvmThread`; `thread_id` is `Copy`.
        .map(|current| unsafe { (*current).thread_id.0 });
    let tid = if thread == 0 {
        None
    } else {
        match lookup_thread(&shared, thread) {
            Ok(ThreadLookup::Live(tid)) => Some(tid).filter(|&tid| Some(tid) != current),
            Ok(ThreadLookup::Unstarted | ThreadLookup::Dead) => return ERR_THREAD_NOT_ALIVE,
            Err(code) => return code,
        }
    };
    let nanos = match tid {
        None => current_thread_cpu_nanos(),
        Some(tid) => shared
            .threads
            .thread_registry
            .os_tid_of(ThreadId(tid))
            .and_then(cratonvm_native_builtins::phases_late::management::os_thread_cpu_time_ns)
            .map(|(cpu, _)| cpu),
    };
    let Some(nanos) = nanos else {
        return ERR_INTERNAL;
    };
    // SAFETY: the agent's non-null out-parameter.
    unsafe { *nanos_ptr = nanos };
    ERR_NONE
}

/// Slot 15: `GetThreadGroupChildren(env, group, thread_count_ptr,
/// threads_ptr, group_count_ptr, groups_ptr)`: the live threads whose group
/// is `group` and its subgroups (`ThreadGroup.subgroups()`'s, as JDWP's
/// `ThreadGroupReference.Children` lists them), as local references in two
/// `Allocate` blocks. A handle that names no thread group answers
/// `JVMTI_ERROR_INTERNAL`, as HotSpot answers it for a `Thread` (measured).
pub(in crate::jvmti::native_env) extern "C" fn get_thread_group_children(
    env: *mut JvmtiNativeEnv,
    group: JObject,
    thread_count_ptr: *mut JInt,
    threads_ptr: *mut *mut JObject,
    group_count_ptr: *mut JInt,
    groups_ptr: *mut *mut JObject,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    if thread_count_ptr.is_null()
        || threads_ptr.is_null()
        || group_count_ptr.is_null()
        || groups_ptr.is_null()
    {
        return ERR_NULL_POINTER;
    }
    let answer = on_calling_thread(&shared, |ctx| {
        let group = jni::jobject_to_obj(group)
            .filter(|&obj| is_thread_group_object(&shared, obj))
            .ok_or(ERR_INTERNAL)?;
        // Interpreter round i5 wave 102, lane L1: platform threads only, in
        // attach order ([`live_platform_thread_objects`]).
        let threads: Vec<JObject> = live_platform_thread_objects(&shared)
            .into_iter()
            .filter(|&t| {
                crate::debug::inspect::group_of_thread(ctx, t)
                    .is_some_and(|g| g.as_ptr() == group.as_ptr())
            })
            .map(jni::new_local_handle)
            .collect();
        let groups: Vec<JObject> = crate::debug::inspect::group_subgroups(&shared, ctx, group)
            .into_iter()
            .map(jni::new_local_handle)
            .collect();
        Ok((threads, groups))
    });
    let (threads, groups) = match answer {
        Ok(Ok(children)) => children,
        Ok(Err(code)) | Err(code) => return code,
    };
    match write_id_array(thread_count_ptr, threads_ptr, &threads) {
        ERR_NONE => {}
        code => return code,
    }
    write_id_array(group_count_ptr, groups_ptr, &groups)
}

/// Slot 91: `IsMethodObsolete(env, method, is_obsolete_ptr)`: false for
/// every method a `jmethodID` names, which is always its class's current
/// method of that name and descriptor (as JDWP's `Method.IsObsolete`
/// answers for every id but the null one); a lambda proxy's too.
pub(in crate::jvmti::native_env) extern "C" fn is_method_obsolete(
    env: *mut JvmtiNativeEnv,
    method: u64,
    is_obsolete_ptr: *mut u8,
) -> JInt {
    let shared = match live_env(env).and_then(|e| e.start_or_live_vm().ok_or(ERR_WRONG_PHASE)) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let known = with_method(&shared, method, |_, _| ());
    let answer = or_proxy_method(&shared, method, known, |_, _| ())
        .map(|()| false)
        .ok_or(ERR_INVALID_METHODID);
    write_jboolean(is_obsolete_ptr, answer)
}

