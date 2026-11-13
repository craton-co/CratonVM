//! Rust port of tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.c: the
//! same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 1 << 16;

static mut AGENT: Env = std::ptr::null_mut();
static mut STEPS: Vec<(String, i32)> = Vec::new();

unsafe extern "system" fn on_single_step(jvmti: Env, _env: Env, _thread: J, method: J, _location: i64) {
    let mut name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        method,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if err != 0 || name.is_null() {
        return;
    }
    let name = agent::take(jvmti, name);
    if let Some(entry) = STEPS.iter_mut().find(|(n, _)| *n == name) {
        entry.1 += 1;
    } else if STEPS.len() < 64 {
        STEPS.push((name, 1));
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [CAN_GENERATE_SINGLE_STEP_EVENTS, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(jvmti, &[(60, on_single_step as usize)]);
    AGENT = jvmti;
    0
}

unsafe fn current_thread() -> J {
    let mut current: J = 0;
    jvmti!(AGENT, 18, extern "system" fn(Env, *mut J) -> i32)(AGENT, &mut current);
    current
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiStepIntoIntrinsic_startStep(_env: Env, _self: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    STEPS.clear();
    let current = current_thread();
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, 1, 60, current)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiStepIntoIntrinsic_stopStep(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let current = current_thread();
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, 0, 60, current);
    let mut out = String::new();
    for (name, count) in STEPS.iter() {
        out.push_str(&format!("{name}={count} "));
    }
    STEPS.clear();
    agent::new_string(env, &out)
}
