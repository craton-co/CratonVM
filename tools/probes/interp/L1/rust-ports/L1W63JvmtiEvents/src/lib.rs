//! Rust port of tools/probes/interp/L1/L1W63JvmtiEvents.c: the same calls
//! in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static GC_STARTS: AtomicI32 = AtomicI32::new(0);
static GC_FINISHES: AtomicI32 = AtomicI32::new(0);
static SAMPLED: AtomicI32 = AtomicI32::new(0);
static GENERATED: AtomicI32 = AtomicI32::new(0);

fn record(line: String) {
    if let Ok(mut lines) = LINES.lock() {
        lines.push(line);
    }
}

fn c(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

/// The name of thread `t` (`GetThreadInfo`), or "?".
unsafe fn thread_name(jvmti: Env, t: J) -> String {
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
    if t == 0 || jvmti!(jvmti, 9, extern "system" fn(Env, J, *mut Info) -> i32)(jvmti, t, &mut info) != 0 {
        return "?".into();
    }
    agent::take(jvmti, info.name)
}

unsafe fn method_name(jvmti: Env, m: J) -> String {
    let mut name: *mut c_char = std::ptr::null_mut();
    if jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        m,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    ) != 0
    {
        return "?".into();
    }
    agent::take(jvmti, name)
}

unsafe fn tag_of(jvmti: Env, o: J) -> i64 {
    let mut tag: i64 = 0;
    jvmti!(jvmti, 106, extern "system" fn(Env, J, *mut i64) -> i32)(jvmti, o, &mut tag);
    tag
}

unsafe fn field_name(jvmti: Env, klass: J, field: J) -> String {
    let mut name: *mut c_char = std::ptr::null_mut();
    if jvmti!(jvmti, 60, extern "system" fn(Env, J, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        klass,
        field,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    ) != 0
    {
        return "?".into();
    }
    agent::take(jvmti, name)
}

// --- callbacks ---------------------------------------------------------------

unsafe extern "system" fn on_field_access(jvmti: Env, _jni: Env, t: J, m: J, loc: i64, klass: J, obj: J, field: J) {
    record(format!(
        "access {} in {} at {loc} by {} object={}",
        field_name(jvmti, klass, field),
        method_name(jvmti, m),
        thread_name(jvmti, t),
        if obj == 0 { "null" } else { "set" }
    ));
}

unsafe extern "system" fn on_field_modification(
    jvmti: Env,
    _jni: Env,
    t: J,
    m: J,
    loc: i64,
    klass: J,
    obj: J,
    field: J,
    sig: c_char,
    value: u64,
) {
    let shown = match sig as u8 {
        b'I' => (value as u32 as i32).to_string(),
        b'J' => (value as i64).to_string(),
        b'L' => if value == 0 { "null".into() } else { "object".into() },
        other => format!("{}:{value:#x}", other as char),
    };
    record(format!(
        "modify {} in {} at {loc} by {} object={} sig={} value={shown}",
        field_name(jvmti, klass, field),
        method_name(jvmti, m),
        thread_name(jvmti, t),
        if obj == 0 { "null" } else { "set" },
        sig as u8 as char
    ));
}

unsafe extern "system" fn replacement(_env: Env, _cls: J) -> i32 {
    77
}

unsafe extern "system" fn on_native_bind(jvmti: Env, _jni: Env, _t: J, m: J, address: *mut c_void, new_address: *mut *mut c_void) {
    let mut klass: J = 0;
    jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, m, &mut klass);
    let sig = agent::signature_of(jvmti, klass);
    if sig != "LL1W63JvmtiEvents;" {
        return;
    }
    let name = method_name(jvmti, m);
    let own = match name.as_str() {
        "boundLater" => Java_L1W63JvmtiEvents_boundLater as *const () as usize,
        "registered" => registered_impl as *const () as usize,
        "replaced" => Java_L1W63JvmtiEvents_replaced as *const () as usize,
        _ => 0,
    };
    if name == "replaced" && !new_address.is_null() {
        *new_address = replacement as *const () as *mut c_void;
    }
    record(format!("bind {name} address-is-own={}", u8::from(address as usize == own)));
}

unsafe extern "system" fn on_compiled_load(jvmti: Env, m: J, size: i32, _addr: *const c_void, _n: i32, _map: *const c_void, _info: *const c_void) {
    GENERATED.fetch_add(1, Ordering::SeqCst);
    let name = method_name(jvmti, m);
    if name == "hot" && size > 0 {
        record("load hot".into());
    }
}

unsafe extern "system" fn on_dynamic_code(_jvmti: Env, name: *const c_char, _addr: *const c_void, length: i32) {
    if !name.is_null() && length > 0 {
        GENERATED.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe fn monitor_line(jvmti: Env, kind: &str, t: J, o: J, extra: String) {
    if tag_of(jvmti, o) == 1 {
        record(format!("monitor {kind} by {}{extra}", thread_name(jvmti, t)));
    }
}

unsafe extern "system" fn on_monitor_wait(jvmti: Env, _jni: Env, t: J, o: J, timeout: i64) {
    monitor_line(jvmti, "wait", t, o, format!(" timeout={timeout}"));
}

unsafe extern "system" fn on_monitor_waited(jvmti: Env, _jni: Env, t: J, o: J, timed_out: u8) {
    monitor_line(jvmti, "waited", t, o, format!(" timed_out={timed_out}"));
}

unsafe extern "system" fn on_contended_enter(jvmti: Env, _jni: Env, t: J, o: J) {
    monitor_line(jvmti, "contended-enter", t, o, String::new());
}

unsafe extern "system" fn on_contended_entered(jvmti: Env, _jni: Env, t: J, o: J) {
    monitor_line(jvmti, "contended-entered", t, o, String::new());
}

unsafe extern "system" fn on_exhausted(_jvmti: Env, _jni: Env, flags: i32, _reserved: *const c_void, description: *const c_char) {
    let text = if description.is_null() {
        "null".to_string()
    } else {
        CStr::from_ptr(description).to_string_lossy().into_owned()
    };
    record(format!("exhausted flags={flags} description={text}"));
}

unsafe extern "system" fn on_gc_start(_jvmti: Env) {
    GC_STARTS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "system" fn on_gc_finish(_jvmti: Env) {
    GC_FINISHES.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "system" fn on_object_free(_jvmti: Env, tag: i64) {
    record(format!("free tag={tag}"));
}

unsafe extern "system" fn on_vm_object_alloc(jvmti: Env, _jni: Env, _t: J, _o: J, klass: J, size: i64) {
    let sig = agent::signature_of(jvmti, klass);
    if sig.contains("Made") || sig == "[I" || sig == "[[I" {
        record(format!("vm-alloc {sig} size={size}"));
    }
}

unsafe extern "system" fn on_sampled(jvmti: Env, _jni: Env, _t: J, _o: J, klass: J, _size: i64) {
    if agent::signature_of(jvmti, klass).contains("Sampled") {
        SAMPLED.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe fn vt_line(jvmti: Env, kind: &str, vt: J) {
    let name = thread_name(jvmti, vt);
    if name == "l1w63-vt" {
        record(format!("vt {kind}"));
    }
}

unsafe extern "system" fn on_vt_start(jvmti: Env, _jni: Env, vt: J) {
    vt_line(jvmti, "start", vt);
}

unsafe extern "system" fn on_vt_end(jvmti: Env, _jni: Env, vt: J) {
    vt_line(jvmti, "end", vt);
}

unsafe extern "C" fn on_vt_mount(jvmti: Env, _jni: Env, vt: J) {
    vt_line(jvmti, "mount", vt);
}

unsafe extern "C" fn on_vt_unmount(jvmti: Env, _jni: Env, vt: J) {
    vt_line(jvmti, "unmount", vt);
}

unsafe extern "C" fn on_class_unload(_jvmti: Env, _jni: Env, name: *const c_char) {
    let text = if name.is_null() {
        "null".to_string()
    } else {
        CStr::from_ptr(name).to_string_lossy().into_owned()
    };
    if text.contains("L1W63") {
        record(format!("unload {text}"));
    }
}

unsafe extern "system" fn on_thread(jvmti: Env, _jni: Env, t: J, start: bool) {
    let name = thread_name(jvmti, t);
    if name.starts_with("l1w63-") {
        record(format!("thread {} {name}", if start { "start" } else { "end" }));
    }
}

unsafe extern "system" fn on_thread_start(jvmti: Env, jni: Env, t: J) {
    on_thread(jvmti, jni, t, true);
}

unsafe extern "system" fn on_thread_end(jvmti: Env, jni: Env, t: J) {
    on_thread(jvmti, jni, t, false);
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    let mut caps: [u32; 4] = [0; 4];
    jvmti!(jvmti, 140, extern "system" fn(Env, *mut [u32; 4]) -> i32)(jvmti, &mut caps);
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &caps);
    let f = |p: usize| p;
    agent::set_event_callbacks(
        jvmti,
        &[
            (52, f(on_thread_start as *const () as usize)),
            (53, f(on_thread_end as *const () as usize)),
            (63, f(on_field_access as *const () as usize)),
            (64, f(on_field_modification as *const () as usize)),
            (67, f(on_native_bind as *const () as usize)),
            (68, f(on_compiled_load as *const () as usize)),
            (70, f(on_dynamic_code as *const () as usize)),
            (73, f(on_monitor_wait as *const () as usize)),
            (74, f(on_monitor_waited as *const () as usize)),
            (75, f(on_contended_enter as *const () as usize)),
            (76, f(on_contended_entered as *const () as usize)),
            (80, f(on_exhausted as *const () as usize)),
            (81, f(on_gc_start as *const () as usize)),
            (82, f(on_gc_finish as *const () as usize)),
            (83, f(on_object_free as *const () as usize)),
            (84, f(on_vm_object_alloc as *const () as usize)),
            (86, f(on_sampled as *const () as usize)),
            (87, f(on_vt_start as *const () as usize)),
            (88, f(on_vt_end as *const () as usize)),
        ],
    );
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
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_enable(_env: Env, _cls: J, event: i32, on: u8) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, if on != 0 { 1 } else { 0 }, event, 0)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_drain(env: Env, _cls: J) -> J {
    need_agent!(env);
    let mut out = String::new();
    let starts = GC_STARTS.swap(0, Ordering::SeqCst);
    let finishes = GC_FINISHES.swap(0, Ordering::SeqCst);
    if starts != 0 || finishes != 0 {
        out.push_str(&format!(
            "gc started={} finished={} balanced={}\n",
            u8::from(starts > 0),
            u8::from(finishes > 0),
            u8::from(starts == finishes)
        ));
    }
    let sampled = SAMPLED.swap(0, Ordering::SeqCst);
    if sampled != 0 {
        out.push_str("sampled Sampled: posted\n");
    }
    if let Ok(mut lines) = LINES.lock() {
        let mut seen: Vec<String> = Vec::new();
        for line in lines.drain(..) {
            // `load hot` once.
            if line == "load hot" && seen.contains(&line) {
                continue;
            }
            seen.push(line.clone());
            out.push_str(&line);
            out.push('\n');
        }
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_watch(
    env: Env,
    _cls: J,
    klass: J,
    name: J,
    sig: J,
    is_static: u8,
    access: u8,
    set: u8,
) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    let name = c(&jstr(env, name));
    let sig = c(&jstr(env, sig));
    let field = jni!(env, if is_static != 0 { 144 } else { 94 }, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        klass,
        name.as_ptr(),
        sig.as_ptr(),
    );
    let slot = match (access != 0, set != 0) {
        (true, true) => 41,
        (true, false) => 42,
        (false, true) => 43,
        (false, false) => 44,
    };
    jvmti!(AGENT, slot, extern "system" fn(Env, J, J) -> i32)(AGENT, klass, field)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_generate(env: Env, _cls: J, event: i32) -> J {
    need_agent!(env);
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, 1, event, 0);
    GENERATED.store(0, Ordering::SeqCst);
    let e = jvmti!(AGENT, 123, extern "system" fn(Env, i32) -> i32)(AGENT, event);
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, 0, event, 0);
    let n = GENERATED.swap(0, Ordering::SeqCst);
    agent::new_string(env, &format!("err={e} posted={}", u8::from(n > 0)))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_sampling(_env: Env, _cls: J, n: i32) -> i32 {
    jvmti!(AGENT, 156, extern "system" fn(Env, i32) -> i32)(AGENT, n)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_extension(_env: Env, _cls: J, index: i32, on: u8) -> i32 {
    let f: usize = if on == 0 {
        0
    } else {
        match index {
            49 => on_class_unload as *const () as usize,
            48 => on_vt_mount as *const () as usize,
            _ => on_vt_unmount as *const () as usize,
        }
    };
    jvmti!(AGENT, 126, extern "system" fn(Env, i32, usize) -> i32)(AGENT, index, f)
}

unsafe extern "system" fn agent_body(_jvmti: Env, _jni: Env, _arg: *mut c_void) {}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_runAgent(_env: Env, _cls: J, t: J) -> i32 {
    jvmti!(AGENT, 12, extern "system" fn(Env, J, usize, *const c_void, i32) -> i32)(
        AGENT,
        t,
        agent_body as *const () as usize,
        std::ptr::null(),
        5,
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_jniAlloc(env: Env, _cls: J, klass: J, construct: u8) -> J {
    if construct == 0 {
        return jni!(env, 27, extern "system" fn(Env, J) -> J)(env, klass);
    }
    let init = jni!(env, 33, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        klass,
        c("<init>").as_ptr(),
        c("()V").as_ptr(),
    );
    jni!(env, 30, extern "system" fn(Env, J, J, *const u64) -> J)(env, klass, init, std::ptr::null())
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_boundLater(_env: Env, _cls: J, x: i32) -> i32 {
    x + 1
}

unsafe extern "system" fn registered_impl(_env: Env, _cls: J) -> i32 {
    55
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_registerNow(env: Env, cls: J) -> i32 {
    #[repr(C)]
    struct NativeMethod {
        name: *const c_char,
        signature: *const c_char,
        fn_ptr: *const c_void,
    }
    let name = c("registered");
    let sig = c("()I");
    let methods = [NativeMethod {
        name: name.as_ptr(),
        signature: sig.as_ptr(),
        fn_ptr: registered_impl as *const c_void,
    }];
    jni!(env, 215, extern "system" fn(Env, J, *const NativeMethod, i32) -> i32)(env, cls, methods.as_ptr(), 1)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_replaced(_env: Env, _cls: J) -> i32 {
    1
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEvents_tag(_env: Env, _cls: J, o: J, tag: i64) -> i32 {
    jvmti!(AGENT, 107, extern "system" fn(Env, J, i64) -> i32)(AGENT, o, tag)
}

unsafe fn jstr(env: Env, s: J) -> String {
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, s, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, s, chars);
    text
}
