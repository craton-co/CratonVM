//! Rust port of tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_get_current_thread_cpu_time` (bit 22), `can_get_thread_cpu_time`
/// (bit 23).
const CURRENT: u32 = 1 << 22;
const THREAD: u32 = 1 << 23;

static mut AGENT: Env = std::ptr::null_mut();

type Caps = [u32; 4];

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: Caps = [CURRENT | THREAD, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &wanted);
    AGENT = jvmti;
    0
}

unsafe fn err_line(env: Env, label: J, err: i32) -> J {
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, label, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, label, chars);
    agent::new_string(env, &format!("{text}: err={err}\n"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiVirtualCpuTime_cpu(env: Env, _self: J, label: J, t: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut nanos: i64 = 0;
    let err = jvmti!(jvmti, 137, extern "system" fn(Env, J, *mut i64) -> i32)(jvmti, t, &mut nanos);
    err_line(env, label, err)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiVirtualCpuTime_currentCpu(env: Env, _self: J, label: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut nanos: i64 = 0;
    let err = jvmti!(jvmti, 135, extern "system" fn(Env, *mut i64) -> i32)(jvmti, &mut nanos);
    err_line(env, label, err)
}
