//! Rust port of tools/probes/interp/L1/L1W55AttachNativeAgent.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

static mut OUT: String = String::new();
static mut ATTACHES: i32 = 0;

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnAttach(vm: *mut *const usize, options: *mut c_char, _r: *mut c_void) -> i32 {
    ATTACHES += 1;
    let mut phase: i32 = 0;
    let got = match agent::get_env(vm, agent::JVMTI_VERSION_1_2) {
        Some(jvmti) => {
            jvmti!(jvmti, 133, extern "system" fn(Env, *mut i32) -> i32)(jvmti, &mut phase);
            jvmti!(jvmti, 127, extern "system" fn(Env) -> i32)(jvmti);
            0
        }
        None => -1,
    };
    let options = if options.is_null() {
        None
    } else {
        Some(CStr::from_ptr(options).to_string_lossy().into_owned())
    };
    line(format!(
        "onattach {ATTACHES}: options={} getenv={got} phase={phase}",
        options.as_deref().unwrap_or("NULL")
    ));
    if options.as_deref() == Some("fail") {
        3
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W55AttachNativeAgent_rows(env: Env, _self: J) -> J {
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
