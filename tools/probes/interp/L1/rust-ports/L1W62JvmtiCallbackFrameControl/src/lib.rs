//! Rust port of tools/probes/interp/L1/L1W62JvmtiCallbackFrameControl.c: the
//! same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
const CAN_POP_FRAME: u32 = 1 << 8;
const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 1 << 16;
const CAN_GENERATE_BREAKPOINT_EVENTS: u32 = 1 << 19;
const CAN_GENERATE_METHOD_ENTRY_EVENTS: u32 = 1 << 24;
/// Word 1.
const CAN_FORCE_EARLY_RETURN: u32 = 1 << 1;
const EVENT_SINGLE_STEP: i32 = 60;
const EVENT_BREAKPOINT: i32 = 62;
const EVENT_METHOD_ENTRY: i32 = 65;

type Caps = [u32; 4];

static mut AGENT: Env = std::ptr::null_mut();
static mut ARMED_EVENT: i32 = 0;
static mut TARGET_NAME: String = String::new();
static mut TARGET_LOCATION: i64 = -1;
static mut ACTIONS: String = String::new();
static mut BP_METHOD: J = 0;
static mut BP_LOCATION: i64 = 0;
static mut HITS: i32 = 0;
static mut ACTED: bool = false;
static mut RECORD: String = String::new();

unsafe fn name_of(jvmti: Env, method: J) -> Option<String> {
    let mut name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        method,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if err != 0 || name.is_null() {
        return None;
    }
    Some(agent::take(jvmti, name))
}

unsafe fn act(jvmti: Env, env: Env, thread: J, method: J, location: i64) {
    let Some(name) = name_of(jvmti, method) else {
        return;
    };
    let matched = name == TARGET_NAME && (TARGET_LOCATION < 0 || location == TARGET_LOCATION);
    if !matched {
        return;
    }
    HITS += 1;
    if ACTED {
        return;
    }
    ACTED = true;
    let actions = ACTIONS.clone();
    for token in actions.split(' ').filter(|t| !t.is_empty()) {
        let err = if let Some(v) = token.strip_prefix("int:") {
            jvmti!(jvmti, 82, extern "system" fn(Env, J, i32) -> i32)(jvmti, thread, v.parse().unwrap_or(0))
        } else if let Some(v) = token.strip_prefix("intnull:") {
            jvmti!(jvmti, 82, extern "system" fn(Env, J, i32) -> i32)(jvmti, 0, v.parse().unwrap_or(0))
        } else if token == "long" {
            jvmti!(jvmti, 83, extern "system" fn(Env, J, i64) -> i32)(jvmti, thread, 77)
        } else if token == "obj" {
            let forced = agent::new_string(env, "forced");
            jvmti!(jvmti, 81, extern "system" fn(Env, J, J) -> i32)(jvmti, thread, forced)
        } else if token == "void" {
            jvmti!(jvmti, 86, extern "system" fn(Env, J) -> i32)(jvmti, thread)
        } else if token == "pop" {
            jvmti!(jvmti, 80, extern "system" fn(Env, J) -> i32)(jvmti, thread)
        } else {
            -1
        };
        RECORD.push_str(&format!("{token}={err} "));
    }
}

unsafe extern "system" fn on_breakpoint(jvmti: Env, env: Env, thread: J, method: J, location: i64) {
    if ARMED_EVENT == 1 {
        act(jvmti, env, thread, method, location);
    }
}

unsafe extern "system" fn on_single_step(jvmti: Env, env: Env, thread: J, method: J, location: i64) {
    if ARMED_EVENT == 2 {
        act(jvmti, env, thread, method, location);
    }
}

unsafe extern "system" fn on_method_entry(jvmti: Env, env: Env, thread: J, method: J) {
    if ARMED_EVENT == 3 {
        act(jvmti, env, thread, method, -1);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: Caps = [
        CAN_POP_FRAME
            | CAN_GENERATE_BREAKPOINT_EVENTS
            | CAN_GENERATE_SINGLE_STEP_EVENTS
            | CAN_GENERATE_METHOD_ENTRY_EVENTS,
        CAN_FORCE_EARLY_RETURN,
        0,
        0,
    ];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(
        jvmti,
        &[
            (EVENT_BREAKPOINT, on_breakpoint as usize),
            (EVENT_SINGLE_STEP, on_single_step as usize),
            (EVENT_METHOD_ENTRY, on_method_entry as usize),
        ],
    );
    AGENT = jvmti;
    0
}

unsafe fn current_thread() -> J {
    let mut current: J = 0;
    jvmti!(AGENT, 18, extern "system" fn(Env, *mut J) -> i32)(AGENT, &mut current);
    current
}

unsafe fn utf(env: Env, s: J) -> String {
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, s, std::ptr::null_mut());
    let out = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, s, chars);
    out
}

unsafe fn set_mode(mode: i32, event: i32, thread: J) -> i32 {
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, mode, event, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W62JvmtiCallbackFrameControl_arm(
    env: Env,
    this: J,
    event: i32,
    method: J,
    sig: J,
    location: i64,
    acts: J,
) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    let m = utf(env, method);
    let s = utf(env, sig);
    TARGET_NAME = m.clone();
    ACTIONS = utf(env, acts);
    TARGET_LOCATION = location;
    HITS = 0;
    ACTED = false;
    RECORD.clear();
    BP_METHOD = 0;
    ARMED_EVENT = event;
    let current = current_thread();
    if event == 1 {
        let m = std::ffi::CString::new(m).unwrap_or_default();
        let s = std::ffi::CString::new(s).unwrap_or_default();
        BP_METHOD = jni!(env, 113, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
            env,
            this,
            m.as_ptr(),
            s.as_ptr(),
        );
        BP_LOCATION = location;
        let mut err = jvmti!(AGENT, 38, extern "system" fn(Env, J, i64) -> i32)(AGENT, BP_METHOD, BP_LOCATION);
        if err == 0 {
            err = set_mode(1, EVENT_BREAKPOINT, current);
        }
        err
    } else if event == 2 {
        set_mode(1, EVENT_SINGLE_STEP, current)
    } else {
        set_mode(1, EVENT_METHOD_ENTRY, current)
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W62JvmtiCallbackFrameControl_disarm(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let current = current_thread();
    if ARMED_EVENT == 1 {
        set_mode(0, EVENT_BREAKPOINT, current);
        jvmti!(AGENT, 39, extern "system" fn(Env, J, i64) -> i32)(AGENT, BP_METHOD, BP_LOCATION);
    } else if ARMED_EVENT == 2 {
        set_mode(0, EVENT_SINGLE_STEP, current);
    } else {
        set_mode(0, EVENT_METHOD_ENTRY, current);
    }
    ARMED_EVENT = 0;
    let out = format!("{}hits={}", RECORD, HITS);
    agent::new_string(env, &out)
}
