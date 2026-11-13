//! Rust port of tools/probes/interp/L1/L1W63JvmtiAgentFunctions.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr, CString};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static mut VM: *mut *const usize = std::ptr::null_mut();
static mut ONLOAD_OUT: String = String::new();

/// The `jvmtiCapabilities` bit fields in declaration order.
const CAPS: [&str; 45] = [
    "can_tag_objects",
    "can_generate_field_modification_events",
    "can_generate_field_access_events",
    "can_get_bytecodes",
    "can_get_synthetic_attribute",
    "can_get_owned_monitor_info",
    "can_get_current_contended_monitor",
    "can_get_monitor_info",
    "can_pop_frame",
    "can_redefine_classes",
    "can_signal_thread",
    "can_get_source_file_name",
    "can_get_line_numbers",
    "can_get_source_debug_extension",
    "can_access_local_variables",
    "can_maintain_original_method_order",
    "can_generate_single_step_events",
    "can_generate_exception_events",
    "can_generate_frame_pop_events",
    "can_generate_breakpoint_events",
    "can_suspend",
    "can_redefine_any_class",
    "can_get_current_thread_cpu_time",
    "can_get_thread_cpu_time",
    "can_generate_method_entry_events",
    "can_generate_method_exit_events",
    "can_generate_all_class_hook_events",
    "can_generate_compiled_method_load_events",
    "can_generate_monitor_events",
    "can_generate_vm_object_alloc_events",
    "can_generate_native_method_bind_events",
    "can_generate_garbage_collection_events",
    "can_generate_object_free_events",
    "can_force_early_return",
    "can_get_owned_monitor_stack_depth_info",
    "can_get_constant_pool",
    "can_set_native_method_prefix",
    "can_retransform_classes",
    "can_retransform_any_class",
    "can_generate_resource_exhaustion_heap_events",
    "can_generate_resource_exhaustion_threads_events",
    "can_generate_early_vmstart",
    "can_generate_early_class_hook_events",
    "can_generate_sampled_object_alloc_events",
    "can_support_virtual_threads",
];

type Caps = [u32; 4];

unsafe fn potential(jvmti: Env) -> (i32, Caps) {
    let mut c: Caps = [0; 4];
    let e = jvmti!(jvmti, 140, extern "system" fn(Env, *mut Caps) -> i32)(jvmti, &mut c);
    (e, c)
}

fn names(c: Caps) -> String {
    let mut out = Vec::new();
    for (i, name) in CAPS.iter().enumerate() {
        if c[i / 32] & (1 << (i % 32)) != 0 {
            out.push(*name);
        }
    }
    out.join(" ")
}

fn c(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

unsafe fn s(p: *const c_char) -> String {
    if p.is_null() {
        "NULL".into()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    VM = vm;
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    let mut out = String::new();
    let (e, p) = potential(jvmti);
    out.push_str(&format!("onload potential: err={e} {}\n", names(p)));
    let e = jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &p);
    out.push_str(&format!("onload add all: err={e}\n"));
    let e = jvmti!(jvmti, 156, extern "system" fn(Env, i32) -> i32)(jvmti, 1024);
    out.push_str(&format!("onload SetHeapSamplingInterval 1024: err={e}\n"));
    let mut n: i32 = -1;
    let mut fs: *mut c_void = std::ptr::null_mut();
    let e = jvmti!(jvmti, 124, extern "system" fn(Env, *mut i32, *mut *mut c_void) -> i32)(jvmti, &mut n, &mut fs);
    out.push_str(&format!("onload GetExtensionFunctions: err={e} count={n}\n"));
    ONLOAD_OUT = out;
    0
}

macro_rules! need_agent {
    ($env:expr) => {
        if AGENT.is_null() {
            return agent::new_string($env, &format!("{NO_ENV}\n"));
        }
    };
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_onload(env: Env, _cls: J) -> J {
    need_agent!(env);
    agent::new_string(env, &ONLOAD_OUT)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_caps(env: Env, _cls: J) -> J {
    need_agent!(env);
    let mut out = String::new();
    let mut held: Caps = [0; 4];
    let e = jvmti!(AGENT, 89, extern "system" fn(Env, *mut Caps) -> i32)(AGENT, &mut held);
    out.push_str(&format!("caps held: err={e} {}\n", names(held)));
    match agent::get_env(VM, agent::JVMTI_VERSION_1_2) {
        Some(fresh) => {
            let (e, p) = potential(fresh);
            out.push_str(&format!("caps live: err={e} {}\n", names(p)));
        }
        None => out.push_str("caps live: no env\n"),
    }
    agent::new_string(env, &out)
}

// -------------------------------------------------------------------------
// Extensions

#[repr(C)]
struct ParamInfo {
    name: *const c_char,
    kind: i32,
    base_type: i32,
    null_ok: u8,
}

#[repr(C)]
struct ExtFunction {
    func: usize,
    id: *const c_char,
    short_description: *const c_char,
    param_count: i32,
    params: *const ParamInfo,
    error_count: i32,
    errors: *const i32,
}

#[repr(C)]
struct ExtEvent {
    extension_event_index: i32,
    id: *const c_char,
    short_description: *const c_char,
    param_count: i32,
    params: *const ParamInfo,
}

unsafe fn params(p: *const ParamInfo, n: i32) -> String {
    let mut out = Vec::new();
    for i in 0..n.max(0) as usize {
        let p = &*p.add(i);
        out.push(format!("{}:{}:{}:{}", s(p.name), p.kind, p.base_type, p.null_ok));
    }
    out.join(",")
}

static mut EXT_CALLS: i32 = 0;

unsafe extern "system" fn ext_event_callback(_jvmti: Env) {
    EXT_CALLS += 1;
}

/// The name of thread `t` (`GetThreadInfo`), "null", or "?err".
unsafe fn thread_name(jvmti: Env, t: J) -> String {
    if t == 0 {
        return "null".into();
    }
    #[repr(C)]
    struct Info {
        name: *mut c_char,
        priority: i32,
        is_daemon: u8,
        group: J,
        loader: J,
    }
    let mut info = Info {
        name: std::ptr::null_mut(),
        priority: 0,
        is_daemon: 0,
        group: 0,
        loader: 0,
    };
    let e = jvmti!(jvmti, 9, extern "system" fn(Env, J, *mut Info) -> i32)(jvmti, t, &mut info);
    if e != 0 {
        return format!("?{e}");
    }
    agent::take(jvmti, info.name)
}

/// `Thread.isVirtual()` of `t`.
unsafe fn is_virtual(env: Env, t: J) -> bool {
    let cls = jni!(env, 31, extern "system" fn(Env, J) -> J)(env, t);
    let name = c("isVirtual");
    let sig = c("()Z");
    let mid = jni!(env, 33, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(env, cls, name.as_ptr(), sig.as_ptr());
    jni!(env, 39, extern "system" fn(Env, J, J, *const u64) -> u8)(env, t, mid, std::ptr::null()) != 0
}

type ExtCall2 = extern "system" fn(Env, J, *mut J) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_extensions(env: Env, _cls: J, platform: J, parked: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let mut out = String::new();
    let mut n: i32 = 0;
    let mut fs: *const ExtFunction = std::ptr::null();
    let e = jvmti!(jvmti, 124, extern "system" fn(Env, *mut i32, *mut *const ExtFunction) -> i32)(jvmti, &mut n, &mut fs);
    out.push_str(&format!("ext functions: err={e} count={n}\n"));
    let mut by_id: Vec<(String, usize)> = Vec::new();
    for i in 0..n.max(0) as usize {
        let f = &*fs.add(i);
        let errors: Vec<String> = (0..f.error_count.max(0) as usize).map(|k| (*f.errors.add(k)).to_string()).collect();
        out.push_str(&format!(
            "ext fn {} [{}] params={} errors={}\n",
            s(f.id),
            s(f.short_description),
            params(f.params, f.param_count),
            errors.join(",")
        ));
        by_id.push((s(f.id), f.func));
    }
    let mut n: i32 = 0;
    let mut evs: *const ExtEvent = std::ptr::null();
    let e = jvmti!(jvmti, 125, extern "system" fn(Env, *mut i32, *mut *const ExtEvent) -> i32)(jvmti, &mut n, &mut evs);
    out.push_str(&format!("ext events: err={e} count={n}\n"));
    for i in 0..n.max(0) as usize {
        let f = &*evs.add(i);
        out.push_str(&format!(
            "ext ev {} {} [{}] params={}\n",
            f.extension_event_index,
            s(f.id),
            s(f.short_description),
            params(f.params, f.param_count)
        ));
    }
    let find = |id: &str| by_id.iter().find(|(name, _)| name == id).map(|&(_, f)| f).unwrap_or(0);
    let unloading = find("com.sun.hotspot.functions.IsClassUnloadingEnabled");
    if unloading != 0 {
        let f: extern "system" fn(Env, *mut u8) -> i32 = std::mem::transmute(unloading);
        let mut v: u8 = 7;
        let e = f(jvmti, &mut v);
        out.push_str(&format!("IsClassUnloadingEnabled: err={e} value={v}\n"));
    }
    let get_virtual = find("com.sun.hotspot.functions.GetVirtualThread");
    let get_carrier = find("com.sun.hotspot.functions.GetCarrierThread");
    if get_virtual != 0 && get_carrier != 0 {
        let gv: ExtCall2 = std::mem::transmute(get_virtual);
        let gc: ExtCall2 = std::mem::transmute(get_carrier);
        let mut t: J = 1;
        let e = gv(jvmti, platform, &mut t);
        out.push_str(&format!("GetVirtualThread platform: err={e} vthread={}\n", thread_name(jvmti, t)));
        let mut t: J = 1;
        let e = gc(jvmti, parked, &mut t);
        out.push_str(&format!("GetCarrierThread parked: err={e} carrier={}\n", thread_name(jvmti, t)));
        let mut t: J = 1;
        let e = gc(jvmti, platform, &mut t);
        out.push_str(&format!("GetCarrierThread platform: err={e}\n"));
        let mut t: J = 1;
        let e = gv(jvmti, parked, &mut t);
        out.push_str(&format!("GetVirtualThread parked: err={e}\n"));
        let e = gv(jvmti, platform, std::ptr::null_mut());
        out.push_str(&format!("GetVirtualThread null out: err={e}\n"));
        let e = gc(jvmti, 0, &mut t);
        out.push_str(&format!("GetCarrierThread null thread: err={e}\n"));
    }
    type SetExt = extern "system" fn(Env, i32, usize) -> i32;
    for index in [46, 47, 48, 49, 50] {
        let e = jvmti!(jvmti, 126, SetExt)(jvmti, index, ext_event_callback as *const () as usize);
        out.push_str(&format!("SetExtensionEventCallback {index}: err={e}\n"));
        jvmti!(jvmti, 126, SetExt)(jvmti, index, 0);
    }
    let e = jvmti!(jvmti, 124, extern "system" fn(Env, *mut i32, *mut *const ExtFunction) -> i32)(jvmti, std::ptr::null_mut(), &mut fs);
    out.push_str(&format!("GetExtensionFunctions null count: err={e}\n"));
    let e = jvmti!(jvmti, 125, extern "system" fn(Env, *mut i32, *mut *const ExtEvent) -> i32)(jvmti, &mut n, std::ptr::null_mut());
    out.push_str(&format!("GetExtensionEvents null array: err={e}\n"));
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_mounted(env: Env, _cls: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let mut out = String::new();
    let mut by_id: Vec<(String, usize)> = Vec::new();
    let mut n: i32 = 0;
    let mut fs: *const ExtFunction = std::ptr::null();
    jvmti!(jvmti, 124, extern "system" fn(Env, *mut i32, *mut *const ExtFunction) -> i32)(jvmti, &mut n, &mut fs);
    for i in 0..n.max(0) as usize {
        let f = &*fs.add(i);
        by_id.push((s(f.id), f.func));
    }
    let find = |id: &str| by_id.iter().find(|(name, _)| name == id).map(|&(_, f)| f).unwrap_or(0);
    let (gv, gc) = (
        find("com.sun.hotspot.functions.GetVirtualThread"),
        find("com.sun.hotspot.functions.GetCarrierThread"),
    );
    if gv == 0 || gc == 0 {
        return agent::new_string(env, "mounted: no extension functions\n");
    }
    let gv: ExtCall2 = std::mem::transmute(gv);
    let gc: ExtCall2 = std::mem::transmute(gc);
    let mut cur: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut cur);
    let mut carrier: J = 0;
    let e = gc(jvmti, cur, &mut carrier);
    out.push_str(&format!(
        "mounted GetCarrierThread: err={e} carrier={} carrier-virtual={}\n",
        if carrier == 0 { "null" } else { "thread" },
        u8::from(carrier != 0 && is_virtual(env, carrier))
    ));
    if carrier != 0 {
        let mut back: J = 0;
        let e = gv(jvmti, carrier, &mut back);
        let same = jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, back, cur);
        out.push_str(&format!("mounted GetVirtualThread carrier: err={e} same={same}\n"));
    }
    agent::new_string(env, &out)
}

// -------------------------------------------------------------------------
// SetVerboseFlag, the JNI function table

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_verbose(_env: Env, _cls: J, flag: i32, value: u8) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 150, extern "system" fn(Env, i32, u8) -> i32)(AGENT, flag, value)
}

const JNI_SLOTS: usize = 236;
static mut SAVED: [usize; JNI_SLOTS] = [0; JNI_SLOTS];
static mut HOOKED: [usize; JNI_SLOTS] = [0; JNI_SLOTS];
static mut VERSION_CALLS: i32 = 0;

unsafe extern "system" fn counting_get_version(env: Env) -> i32 {
    VERSION_CALLS += 1;
    let original: extern "system" fn(Env) -> i32 = std::mem::transmute(SAVED[4]);
    original(env)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_jniTable(env: Env, _cls: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let mut out = String::new();
    let mut table: *const usize = std::ptr::null();
    type GetTable = extern "system" fn(Env, *mut *const usize) -> i32;
    type SetTable = extern "system" fn(Env, *const usize) -> i32;
    let e = jvmti!(jvmti, 121, GetTable)(jvmti, &mut table);
    let mut same = 1;
    if e == 0 {
        for i in 0..JNI_SLOTS {
            if *table.add(i) != *(*env).add(i) {
                same = 0;
            }
            SAVED[i] = *table.add(i);
        }
    }
    out.push_str(&format!("jni get: err={e} same={same} copy={}\n", u8::from(table != *env)));
    out.push_str(&format!("jni get null: err={}\n", jvmti!(jvmti, 121, GetTable)(jvmti, std::ptr::null_mut())));
    out.push_str(&format!("jni set null: err={}\n", jvmti!(jvmti, 120, SetTable)(jvmti, std::ptr::null())));
    HOOKED = SAVED;
    HOOKED[4] = counting_get_version as *const () as usize;
    let e = jvmti!(jvmti, 120, SetTable)(jvmti, HOOKED.as_ptr());
    out.push_str(&format!("jni set: err={e}\n"));
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_jniVersion(env: Env, _cls: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let version = jni!(env, 4, extern "system" fn(Env) -> i32)(env);
    let hooked = VERSION_CALLS;
    let e = jvmti!(jvmti, 120, extern "system" fn(Env, *const usize) -> i32)(jvmti, SAVED.as_ptr());
    jni!(env, 4, extern "system" fn(Env) -> i32)(env);
    agent::new_string(
        env,
        &format!(
            "jni version: {version:#x} hooked-calls={hooked} restore err={e} after-restore={}\n",
            VERSION_CALLS
        ),
    )
}

// -------------------------------------------------------------------------
// Class-file attributes

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_sourceDebugExtension(env: Env, _cls: J, k: J) -> J {
    need_agent!(env);
    let mut text: *mut c_char = std::ptr::null_mut();
    let e = jvmti!(AGENT, 90, extern "system" fn(Env, J, *mut *mut c_char) -> i32)(AGENT, k, &mut text);
    if e != 0 {
        return agent::new_string(env, &format!("!{e}"));
    }
    agent::new_string(env, &format!("={}", agent::take(AGENT, text)))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_constantPool(env: Env, _cls: J, k: J) -> J {
    need_agent!(env);
    let mut count: i32 = 0;
    let mut len: i32 = 0;
    let mut bytes: *mut u8 = std::ptr::null_mut();
    let e = jvmti!(AGENT, 146, extern "system" fn(Env, J, *mut i32, *mut i32, *mut *mut u8) -> i32)(
        AGENT, k, &mut count, &mut len, &mut bytes,
    );
    if e != 0 {
        return agent::new_string(env, &format!("{e}"));
    }
    let mut hex = String::new();
    for i in 0..len.max(0) as usize {
        hex.push_str(&format!("{:02x}", *bytes.add(i)));
    }
    agent::deallocate(AGENT, bytes.cast());
    agent::new_string(env, &format!("0 {count} {hex}"))
}

// -------------------------------------------------------------------------
// Monitors, interrupt, stop

#[repr(C)]
struct MonitorUsage {
    owner: J,
    entry_count: i32,
    waiter_count: i32,
    waiters: *mut J,
    notify_waiter_count: i32,
    notify_waiters: *mut J,
}

unsafe fn sorted_names(jvmti: Env, list: *mut J, n: i32) -> String {
    let mut v: Vec<String> = (0..n.max(0) as usize).map(|i| thread_name(jvmti, *list.add(i))).collect();
    v.sort();
    agent::deallocate(jvmti, list.cast());
    v.join(",")
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_monitorUsage(env: Env, _cls: J, o: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let mut u = MonitorUsage {
        owner: 0,
        entry_count: -1,
        waiter_count: -1,
        waiters: std::ptr::null_mut(),
        notify_waiter_count: -1,
        notify_waiters: std::ptr::null_mut(),
    };
    let e = jvmti!(jvmti, 59, extern "system" fn(Env, J, *mut MonitorUsage) -> i32)(jvmti, o, &mut u);
    if e != 0 {
        return agent::new_string(env, &format!("err={e}"));
    }
    let owner = thread_name(jvmti, u.owner);
    let waiters = sorted_names(jvmti, u.waiters, u.waiter_count);
    let notify = sorted_names(jvmti, u.notify_waiters, u.notify_waiter_count);
    agent::new_string(
        env,
        &format!(
            "err=0 owner={owner} entry_count={} waiters={}[{waiters}] notify_waiters={}[{notify}]",
            u.entry_count, u.waiter_count, u.notify_waiter_count
        ),
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_interrupt(_env: Env, _cls: J, t: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 8, extern "system" fn(Env, J) -> i32)(AGENT, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_interruptString(_env: Env, _cls: J, t: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 8, extern "system" fn(Env, J) -> i32)(AGENT, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_stop(_env: Env, _cls: J, t: J, ex: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 7, extern "system" fn(Env, J, J) -> i32)(AGENT, t, ex)
}

// -------------------------------------------------------------------------
// RunAgentThread

static mut LOCK: J = 0;
static mut DONE: bool = false;
static mut AGENT_THREAD: J = 0;
static mut PROC_OUT: String = String::new();

unsafe extern "system" fn agent_proc(jvmti: Env, env: Env, arg: *mut c_void) {
    #[repr(C)]
    struct Info {
        name: *mut c_char,
        priority: i32,
        is_daemon: u8,
        group: J,
        loader: J,
    }
    let mut info = Info {
        name: std::ptr::null_mut(),
        priority: 0,
        is_daemon: 0,
        group: 0,
        loader: 0,
    };
    let e = jvmti!(jvmti, 9, extern "system" fn(Env, J, *mut Info) -> i32)(jvmti, 0, &mut info);
    let name = if e == 0 { agent::take(jvmti, info.name) } else { format!("?{e}") };
    let mut cur: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut cur);
    let same = jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, cur, AGENT_THREAD);
    let thread_cls = jni!(env, 6, extern "system" fn(Env, *const c_char) -> J)(env, c("java/lang/Thread").as_ptr());
    let mid = jni!(env, 113, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        thread_cls,
        c("currentThread").as_ptr(),
        c("()Ljava/lang/Thread;").as_ptr(),
    );
    let current = jni!(env, 116, extern "system" fn(Env, J, J, *const u64) -> J)(env, thread_cls, mid, std::ptr::null());
    let same_java = jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, current, AGENT_THREAD);
    let mut state: i32 = 0;
    jvmti!(jvmti, 17, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, 0, &mut state);
    let arg_ok = u8::from(arg as usize == 0x1234);
    PROC_OUT = format!(
        "agent proc: name={name} priority={} daemon={} arg={arg_ok} current={same} currentThread={same_java} alive={} runnable={}\n",
        info.priority,
        info.is_daemon,
        state & 1,
        u8::from(state & 4 != 0)
    );
    jvmti!(jvmti, 33, extern "system" fn(Env, J) -> i32)(jvmti, LOCK);
    DONE = true;
    jvmti!(jvmti, 37, extern "system" fn(Env, J) -> i32)(jvmti, LOCK);
    jvmti!(jvmti, 34, extern "system" fn(Env, J) -> i32)(jvmti, LOCK);
}

unsafe extern "system" fn noop_proc(_jvmti: Env, _env: Env, _arg: *mut c_void) {}

type RunAgent = extern "system" fn(Env, J, usize, *const c_void, i32) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_runAgentThread(env: Env, _cls: J, t: J, priority: i32) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    if LOCK == 0 {
        jvmti!(jvmti, 31, extern "system" fn(Env, *const c_char, *mut J) -> i32)(jvmti, c("l1w63").as_ptr(), &mut LOCK);
    }
    AGENT_THREAD = jni!(env, 21, extern "system" fn(Env, J) -> J)(env, t);
    let e = jvmti!(jvmti, 12, RunAgent)(jvmti, t, agent_proc as *const () as usize, 0x1234 as *const c_void, priority);
    let mut out = format!("RunAgentThread: err={e}\n");
    if e == 0 {
        jvmti!(jvmti, 33, extern "system" fn(Env, J) -> i32)(jvmti, LOCK);
        let mut rounds = 0;
        while !DONE && rounds < 50 {
            jvmti!(jvmti, 35, extern "system" fn(Env, J, i64) -> i32)(jvmti, LOCK, 100);
            rounds += 1;
        }
        jvmti!(jvmti, 34, extern "system" fn(Env, J) -> i32)(jvmti, LOCK);
        out.push_str(if DONE { &PROC_OUT } else { "agent proc: did not run\n" });
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_runAgentThreadErrors(env: Env, _cls: J, started: J, fresh: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let not_thread = agent::new_string(env, "x");
    let rows: [(&str, J, usize, i32); 6] = [
        ("null proc", fresh, 0, 5),
        ("priority 0", fresh, noop_proc as *const () as usize, 0),
        ("priority 11", fresh, noop_proc as *const () as usize, 11),
        ("null thread", 0, noop_proc as *const () as usize, 5),
        ("not a thread", not_thread, noop_proc as *const () as usize, 5),
        ("terminated thread", started, noop_proc as *const () as usize, 5),
    ];
    let mut out = String::new();
    for (row, t, f, p) in rows {
        let e = jvmti!(jvmti, 12, RunAgent)(jvmti, t, f, std::ptr::null(), p);
        out.push_str(&format!("RunAgentThread {row}: err={e}\n"));
    }
    agent::new_string(env, &out)
}

// -------------------------------------------------------------------------
// Modules

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_modules(
    env: Env,
    _cls: J,
    base: J,
    logging: J,
    unnamed: J,
    service: J,
    implementation: J,
    not_module: J,
) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let same = |a: J, b: J| jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, a, b);
    let mut out = String::new();
    let mut n: i32 = 0;
    let mut list: *mut J = std::ptr::null_mut();
    let e = jvmti!(jvmti, 3, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(jvmti, &mut n, &mut list);
    let has = |m: J| (0..n.max(0) as usize).any(|i| same(*list.add(i), m) != 0);
    out.push_str(&format!(
        "GetAllModules: err={e} many={} base={} logging={} app-unnamed={}\n",
        u8::from(n > 20),
        u8::from(has(base)),
        u8::from(has(logging)),
        u8::from(has(unnamed))
    ));
    agent::deallocate(jvmti, list.cast());
    out.push_str(&format!(
        "GetAllModules null: err={}\n",
        jvmti!(jvmti, 3, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(jvmti, std::ptr::null_mut(), &mut list)
    ));
    type Named = extern "system" fn(Env, J, *const c_char, *mut J) -> i32;
    for (row, pkg, want) in [("java/lang", "java/lang", base), ("java/util/logging", "java/util/logging", logging), ("no/such", "no/such", 0)] {
        let mut m: J = 1;
        let e = jvmti!(jvmti, 40, Named)(jvmti, 0, c(pkg).as_ptr(), &mut m);
        let what = if m == 0 { "null".to_string() } else { format!("same={}", same(m, want)) };
        out.push_str(&format!("GetNamedModule boot {row}: err={e} {what}\n"));
    }
    let mut m: J = 0;
    out.push_str(&format!("GetNamedModule null package: err={}\n", jvmti!(jvmti, 40, Named)(jvmti, 0, std::ptr::null(), &mut m)));
    out.push_str(&format!(
        "GetNamedModule not a loader: err={}\n",
        jvmti!(jvmti, 40, Named)(jvmti, not_module, c("java/lang").as_ptr(), &mut m)
    ));
    type Reads = extern "system" fn(Env, J, J) -> i32;
    type Exports = extern "system" fn(Env, J, *const c_char, J) -> i32;
    type Provides = extern "system" fn(Env, J, J, J) -> i32;
    out.push_str(&format!("AddModuleReads: err={}\n", jvmti!(jvmti, 94, Reads)(jvmti, logging, unnamed)));
    out.push_str(&format!(
        "AddModuleExports dotted: err={}\n",
        jvmti!(jvmti, 95, Exports)(jvmti, base, c("jdk.internal.misc").as_ptr(), unnamed)
    ));
    out.push_str(&format!(
        "AddModuleExports slashed: err={}\n",
        jvmti!(jvmti, 95, Exports)(jvmti, base, c("jdk/internal/misc").as_ptr(), unnamed)
    ));
    out.push_str(&format!(
        "AddModuleExports not in module: err={}\n",
        jvmti!(jvmti, 95, Exports)(jvmti, base, c("no.such").as_ptr(), unnamed)
    ));
    out.push_str(&format!(
        "AddModuleOpens: err={}\n",
        jvmti!(jvmti, 96, Exports)(jvmti, base, c("jdk.internal.reflect").as_ptr(), unnamed)
    ));
    out.push_str(&format!("AddModuleUses: err={}\n", jvmti!(jvmti, 97, Reads)(jvmti, logging, service)));
    out.push_str(&format!(
        "AddModuleProvides: err={}\n",
        jvmti!(jvmti, 98, Provides)(jvmti, logging, service, implementation)
    ));
    out.push_str(&format!("AddModuleReads not a module: err={}\n", jvmti!(jvmti, 94, Reads)(jvmti, not_module, unnamed)));
    out.push_str(&format!("AddModuleReads null to: err={}\n", jvmti!(jvmti, 94, Reads)(jvmti, logging, 0)));
    out.push_str(&format!("AddModuleUses not a class: err={}\n", jvmti!(jvmti, 97, Reads)(jvmti, logging, not_module)));
    type Modifiable = extern "system" fn(Env, J, *mut u8) -> i32;
    let mut v: u8 = 7;
    let e = jvmti!(jvmti, 99, Modifiable)(jvmti, base, &mut v);
    out.push_str(&format!("IsModifiableModule base: err={e} value={v}\n"));
    let mut v: u8 = 7;
    let e = jvmti!(jvmti, 99, Modifiable)(jvmti, unnamed, &mut v);
    out.push_str(&format!("IsModifiableModule unnamed: err={e} value={v}\n"));
    out.push_str(&format!("IsModifiableModule not a module: err={}\n", jvmti!(jvmti, 99, Modifiable)(jvmti, not_module, &mut v)));
    agent::new_string(env, &out)
}

// -------------------------------------------------------------------------
// Virtual threads, sampling, prefixes

unsafe fn suspended(jvmti: Env, t: J) -> i32 {
    let mut state: i32 = 0;
    let e = jvmti!(jvmti, 17, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, t, &mut state);
    if e != 0 {
        return -e;
    }
    i32::from(state & 0x10_0000 != 0)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_suspendAllVirtual(env: Env, _cls: J, vts: J, except: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let n = jni!(env, 171, extern "system" fn(Env, J) -> i32)(env, vts);
    let at = |i: i32| jni!(env, 173, extern "system" fn(Env, J, i32) -> J)(env, vts, i);
    type All = extern "system" fn(Env, i32, *const J) -> i32;
    let mut out = String::new();
    let row = |out: &mut String, label: &str| {
        let mut line = format!("{label}:");
        for i in 0..n {
            line.push_str(&format!(" vt{i}={}", suspended(jvmti, at(i))));
        }
        out.push_str(&line);
        out.push('\n');
    };
    out.push_str(&format!("SuspendAll negative: err={}\n", jvmti!(jvmti, 118, All)(jvmti, -1, std::ptr::null())));
    out.push_str(&format!("SuspendAll null list: err={}\n", jvmti!(jvmti, 118, All)(jvmti, 1, std::ptr::null())));
    let list = [except];
    let e = jvmti!(jvmti, 118, All)(jvmti, 1, list.as_ptr());
    out.push_str(&format!("SuspendAllVirtualThreads except vt1: err={e}\n"));
    row(&mut out, "suspended");
    let e = jvmti!(jvmti, 119, All)(jvmti, 0, std::ptr::null());
    out.push_str(&format!("ResumeAllVirtualThreads: err={e}\n"));
    row(&mut out, "resumed");
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_sampling(_env: Env, _cls: J, n: i32) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 156, extern "system" fn(Env, i32) -> i32)(AGENT, n)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_prefix(env: Env, _cls: J, p: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    type Prefix = extern "system" fn(Env, *const c_char) -> i32;
    if p == 0 {
        return jvmti!(AGENT, 73, Prefix)(AGENT, std::ptr::null());
    }
    let text = c(&jstr(env, p));
    jvmti!(AGENT, 73, Prefix)(AGENT, text.as_ptr())
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_prefixes(env: Env, _cls: J, arr: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    type Prefixes = extern "system" fn(Env, i32, *const *const c_char) -> i32;
    if arr == 0 {
        return jvmti!(AGENT, 74, Prefixes)(AGENT, -1, std::ptr::null());
    }
    let n = jni!(env, 171, extern "system" fn(Env, J) -> i32)(env, arr);
    let texts: Vec<CString> = (0..n)
        .map(|i| c(&jstr(env, jni!(env, 173, extern "system" fn(Env, J, i32) -> J)(env, arr, i))))
        .collect();
    let ptrs: Vec<*const c_char> = texts.iter().map(|t| t.as_ptr()).collect();
    jvmti!(AGENT, 74, Prefixes)(AGENT, n, ptrs.as_ptr())
}

unsafe fn jstr(env: Env, s: J) -> String {
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, s, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, s, chars);
    text
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_twice(_env: Env, _cls: J, x: i32) -> i32 {
    x * 2
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiAgentFunctions_thrice(_env: Env, _cls: J, x: i32) -> i32 {
    x * 3
}
