//! Rust port of tools/probes/interp/L1/L1W57JvmtiErrorNames.c: the same calls
//! in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W57JvmtiErrorNames_names(env: Env, _self: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let error_name = jvmti!(jvmti, 128, extern "system" fn(Env, i32, *mut *mut c_char) -> i32);
    let mut out = String::new();
    for code in 0..=120 {
        let mut name: *mut c_char = std::ptr::null_mut();
        let err = error_name(jvmti, code, &mut name);
        if err == 0 {
            out.push_str(&format!("{code} {}\n", agent::take(jvmti, name)));
        } else {
            out.push_str(&format!("{code} err={err}\n"));
            agent::deallocate(jvmti, name);
        }
    }
    let null_out = error_name(jvmti, 0, std::ptr::null_mut());
    out.push_str(&format!("null out: err={null_out}\n"));
    agent::new_string(env, &out)
}
