//! The JVMTI shim of tools/probes/interp/L2/I41ObsoleteLockingLoops.java and
//! tools/probes/interp/L2/I41CallerHeldSyncLeafSteps.java
//! (no C twin: written as a Rust port from the start). Loaded twice: as an
//! agent (`-agentpath`), whose `Agent_OnLoad` adds
//! `can_generate_single_step_events` and sets the `SingleStep` callback, and
//! with `System.load`, for the natives. `armStep(thread)` enables
//! `SingleStep` for that thread, `disarmStep(thread)` disables it, and
//! `report()` answers `name=count@first-location ` per stepped method (and
//! clears).
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};
use std::sync::Mutex;

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 1 << 16;
const SINGLE_STEP: i32 = 60;

static mut AGENT: Env = std::ptr::null_mut();
/// Per stepped method: its name, the step count and the location of its first step.
static STEPS: Mutex<Vec<(String, i32, i64)>> = Mutex::new(Vec::new());

unsafe extern "system" fn on_single_step(
    jvmti: Env,
    _env: Env,
    _thread: J,
    method: J,
    location: i64,
) {
    let mut name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(
        jvmti,
        64,
        extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32
    )(
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
    let mut steps = STEPS.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(entry) = steps.iter_mut().find(|(n, _, _)| *n == name) {
        entry.1 += 1;
    } else if steps.len() < 64 {
        steps.push((name, 1, location));
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(
    vm: *mut *const usize,
    _options: *mut c_char,
    _r: *mut c_void,
) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [CAN_GENERATE_SINGLE_STEP_EVENTS, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(jvmti, &[(SINGLE_STEP, on_single_step as usize)]);
    AGENT = jvmti;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_I41ObsoleteLockingLoops_armStep(
    _env: Env,
    _class: J,
    thread: J,
) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    STEPS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, 1, SINGLE_STEP, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Java_I41ObsoleteLockingLoops_disarmStep(
    _env: Env,
    _class: J,
    thread: J,
) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32)(AGENT, 0, SINGLE_STEP, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Java_I41ObsoleteLockingLoops_report(env: Env, _class: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let mut steps = STEPS.lock().unwrap_or_else(|p| p.into_inner());
    let mut out = String::new();
    for (name, count, first) in steps.iter() {
        out.push_str(&format!("{name}={count}@{first} "));
    }
    steps.clear();
    agent::new_string(env, &out)
}

// The same three natives for tools/probes/interp/L2/I41CallerHeldSyncLeafSteps.java.

#[no_mangle]
pub unsafe extern "system" fn Java_I41CallerHeldSyncLeafSteps_armStep(
    env: Env,
    class: J,
    thread: J,
) -> i32 {
    Java_I41ObsoleteLockingLoops_armStep(env, class, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Java_I41CallerHeldSyncLeafSteps_disarmStep(
    env: Env,
    class: J,
    thread: J,
) -> i32 {
    Java_I41ObsoleteLockingLoops_disarmStep(env, class, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Java_I41CallerHeldSyncLeafSteps_report(env: Env, class: J) -> J {
    Java_I41ObsoleteLockingLoops_report(env, class)
}
