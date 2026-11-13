//! Rust port of tools/probes/interp/L1/L1K54JvmtiLateAgentLambdaFrames.c:
//! the same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

static mut AGENT: Env = std::ptr::null_mut();

#[no_mangle]
pub unsafe extern "system" fn Agent_OnAttach(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    0
}

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

type Names = extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLateAgentLambdaFrames_stack(env: Env, _cls: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, "no JVMTI env");
    }
    // `jvmtiFrameInfo`: a `jmethodID` and a `jlocation`.
    let mut frames = [(0usize, 0i64); 8];
    let mut count: i32 = 0;
    let err = jvmti!(jvmti, 104, extern "system" fn(Env, J, i32, i32, *mut (usize, i64), *mut i32) -> i32)(
        jvmti,
        0,
        1,
        5,
        frames.as_mut_ptr(),
        &mut count,
    );
    let mut out = format!("  err={err} count={count}");
    if err == 0 {
        for &(method, location) in frames.iter().take(count.max(0) as usize) {
            let mut declaring: J = 0;
            jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, method, &mut declaring);
            let sig = agent::signature_of(jvmti, declaring);
            let mut name: *mut c_char = std::ptr::null_mut();
            jvmti!(jvmti, 64, Names)(jvmti, method, &mut name, std::ptr::null_mut(), std::ptr::null_mut());
            let name = if name.is_null() { "?".to_string() } else { agent::take(jvmti, name) };
            out.push_str(&format!("\n  {}.{name} at {location}", mask(&sig)));
        }
    }
    agent::new_string(env, &out)
}
