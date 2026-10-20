//! Rust port of tools/probes/interp/L1/L1W56JvmtiReentryContended.c: the
//! same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_get_current_contended_monitor`, the 7th bit field.
const CAN_GET_CURRENT_CONTENDED_MONITOR: u32 = 1 << 6;
/// ALIVE, RUNNABLE, WAITING, BLOCKED_ON_MONITOR_ENTER.
const STATE_MASK: i32 = 0x485;

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD_ADD: i32 = -1;

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [CAN_GET_CURRENT_CONTENDED_MONITOR, 0, 0, 0];
    ONLOAD_ADD = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    AGENT = jvmti;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W56JvmtiReentryContended_contended(
    env: Env,
    _self: J,
    label: J,
    t: J,
    lock: J,
) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut monitor: J = 0;
    let mut state: i32 = 0;
    let err = jvmti!(jvmti, 11, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, t, &mut monitor);
    jvmti!(jvmti, 17, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, t, &mut state);
    let which = if monitor == 0 {
        "null"
    } else if jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, monitor, lock) != 0 {
        "lock"
    } else {
        "other"
    };
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, label, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, label, chars);
    agent::new_string(
        env,
        &format!(
            "{text}: onload={ONLOAD_ADD} err={err} monitor={which} state=0x{:x}\n",
            (state & STATE_MASK) as u32
        ),
    )
}
