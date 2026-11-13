//! Rust port of tools/probes/interp/L1/L1K54JvmtiLambdaProxyEvents.c: the
//! same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

/// `can_generate_method_entry_events` (bit 24) and
/// `can_generate_method_exit_events` (bit 25).
const WANTED: u32 = (1 << 24) | (1 << 25);
const CLASS_FILE_LOAD_HOOK: i32 = 54;
const CLASS_LOAD: i32 = 55;
const CLASS_PREPARE: i32 = 56;
const METHOD_ENTRY: i32 = 65;
const METHOD_EXIT: i32 = 66;

static mut AGENT: Env = std::ptr::null_mut();
static RECORDING: AtomicBool = AtomicBool::new(false);
static mut LINES: String = String::new();

/// `0x` and the hex digits after it as `0x?`.
fn mask(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("0x") {
        out.push_str(&rest[..at]);
        out.push_str("0x?");
        rest = rest[at + 2..].trim_start_matches(|c: char| c.is_ascii_digit() || ('a'..='f').contains(&c));
    }
    out.push_str(rest);
    out
}

unsafe fn note(what: &str, name: &str) {
    LINES.push_str(&format!("{what} {}\n", mask(name)));
}

unsafe fn class_event(jvmti: Env, what: &str, klass: J) {
    if !RECORDING.load(Ordering::SeqCst) {
        return;
    }
    let sig = agent::signature_of(jvmti, klass);
    if sig.contains("$$Lambda") {
        note(what, &sig);
    }
}

unsafe fn method_event(jvmti: Env, what: &str, method: J) {
    if !RECORDING.load(Ordering::SeqCst) {
        return;
    }
    let mut declaring: J = 0;
    if jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, method, &mut declaring) != 0 {
        return;
    }
    let sig = agent::signature_of(jvmti, declaring);
    if !sig.contains("$$Lambda") {
        return;
    }
    let mut name: *mut c_char = std::ptr::null_mut();
    if jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        method,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    ) == 0
        && !name.is_null()
    {
        let name = agent::take(jvmti, name);
        note(what, &format!("{sig}.{name}"));
    }
}

unsafe extern "system" fn on_hook(
    _jvmti: Env,
    _env: Env,
    _redefined: J,
    _loader: J,
    name: *const c_char,
    _pd: J,
    _len: i32,
    _data: *const u8,
    _new_len: *mut i32,
    _new_data: *mut *mut u8,
) {
    if RECORDING.load(Ordering::SeqCst) && !name.is_null() {
        let name = CStr::from_ptr(name).to_string_lossy().into_owned();
        if name.contains("$$Lambda") {
            note("hook", &name);
        }
    }
}

unsafe extern "system" fn on_load(jvmti: Env, _env: Env, _thread: J, klass: J) {
    class_event(jvmti, "load", klass);
}

unsafe extern "system" fn on_prepare(jvmti: Env, _env: Env, _thread: J, klass: J) {
    class_event(jvmti, "prepare", klass);
}

unsafe extern "system" fn on_entry(jvmti: Env, _env: Env, _thread: J, method: J) {
    method_event(jvmti, "entry", method);
}

unsafe extern "system" fn on_exit(jvmti: Env, _env: Env, _thread: J, method: J, _popped: u8, _value: u64) {
    method_event(jvmti, "exit", method);
}

unsafe fn notify(jvmti: Env, mode: i32, event: i32, thread: J) {
    jvmti!(jvmti, 2, extern "C" fn(Env, i32, i32, J) -> i32)(jvmti, mode, event, thread);
}

unsafe fn current_thread(jvmti: Env) -> J {
    let mut thread: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut thread);
    thread
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [WANTED, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(
        jvmti,
        &[
            (CLASS_FILE_LOAD_HOOK, on_hook as *const () as usize),
            (CLASS_LOAD, on_load as *const () as usize),
            (CLASS_PREPARE, on_prepare as *const () as usize),
            (METHOD_ENTRY, on_entry as *const () as usize),
            (METHOD_EXIT, on_exit as *const () as usize),
        ],
    );
    notify(jvmti, 1, CLASS_FILE_LOAD_HOOK, 0);
    notify(jvmti, 1, CLASS_LOAD, 0);
    notify(jvmti, 1, CLASS_PREPARE, 0);
    AGENT = jvmti;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLambdaProxyEvents_start(_env: Env, _cls: J) {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return;
    }
    LINES.clear();
    let thread = current_thread(jvmti);
    notify(jvmti, 1, METHOD_ENTRY, thread);
    notify(jvmti, 1, METHOD_EXIT, thread);
    RECORDING.store(true, Ordering::SeqCst);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLambdaProxyEvents_stop(env: Env, _cls: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, "no JVMTI env\n");
    }
    RECORDING.store(false, Ordering::SeqCst);
    let thread = current_thread(jvmti);
    notify(jvmti, 0, METHOD_ENTRY, thread);
    notify(jvmti, 0, METHOD_EXIT, thread);
    agent::new_string(env, &LINES)
}
