//! Rust port of tools/probes/interp/L1/L1W60JvmtiLambdaFrames.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static mut OUT: String = String::new();

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

/// `text` with every hex digit after "0x" as 'h'.
fn mask(text: &str) -> String {
    let Some(at) = text.find("0x") else {
        return text.to_string();
    };
    let (head, tail) = text.split_at(at + 2);
    let digits = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
    format!("{head}{}{}", "h".repeat(digits), &tail[digits..])
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    OUT.clear();
    AGENT = jvmti;
    0
}

type Names = extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;

/// `Class.method(signature)` of `m`, the class's signature masked.
unsafe fn name_of(jvmti: Env, m: J) -> String {
    let mut declaring: J = 0;
    jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, m, &mut declaring);
    let csig = agent::signature_of(jvmti, declaring);
    let mut name: *mut c_char = std::ptr::null_mut();
    let mut sig: *mut c_char = std::ptr::null_mut();
    jvmti!(jvmti, 64, Names)(jvmti, m, &mut name, &mut sig, std::ptr::null_mut());
    let name = if name.is_null() { "?".to_string() } else { agent::take(jvmti, name) };
    let sig = if sig.is_null() { String::new() } else { agent::take(jvmti, sig) };
    format!("{} {name}{sig}", mask(&csig))
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrameInfo {
    method: J,
    location: i64,
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W60JvmtiLambdaFrames_stack(env: Env, _self: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut frames = [FrameInfo { method: 0, location: 0 }; 6];
    let mut count: i32 = 0;
    let err = jvmti!(jvmti, 104, extern "system" fn(Env, J, i32, i32, *mut FrameInfo, *mut i32) -> i32)(
        jvmti,
        0,
        0,
        6,
        frames.as_mut_ptr(),
        &mut count,
    );
    line(format!("  stack: err={err} count={count}"));
    if err == 0 {
        for i in 1..count.max(0) {
            let frame = frames[i as usize];
            line(format!("    frame {i}: {} at {}", name_of(jvmti, frame.method), frame.location));
        }
    }
    let mut depth: i32 = 0;
    let cerr = jvmti!(jvmti, 16, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, 0, &mut depth);
    let mut at: J = 0;
    let mut location: i64 = 0;
    let lerr = jvmti!(jvmti, 19, extern "system" fn(Env, J, i32, *mut J, *mut i64) -> i32)(
        jvmti,
        0,
        2,
        &mut at,
        &mut location,
    );
    let named = if lerr == 0 { name_of(jvmti, at) } else { "-".to_string() };
    line(format!("  frame count: err={cerr} count={depth}"));
    line(format!(
        "  depth 2: err={lerr} {named} at {}",
        if lerr == 0 { location } else { -1 }
    ));
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}

unsafe fn exception_check(env: Env) -> i32 {
    i32::from(jni!(env, 228, extern "system" fn(Env) -> u8)(env))
}

unsafe fn exception_clear(env: Env) {
    jni!(env, 17, extern "system" fn(Env))(env);
}

/// `GetMethodID(klass, name, sig)`: found, and the declaring class (its
/// signature masked, whether it is `klass`) and modifiers of the answer.
unsafe fn report(env: Env, jvmti: Env, klass: J, name: &str, sig: &str) -> J {
    let c_name = std::ffi::CString::new(name).unwrap_or_default();
    let c_sig = std::ffi::CString::new(sig).unwrap_or_default();
    let m = jni!(env, 33, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        klass,
        c_name.as_ptr(),
        c_sig.as_ptr(),
    );
    if m == 0 {
        let pending = exception_check(env);
        exception_clear(env);
        line(format!("  {name}{sig}: found=0 pending={pending}"));
        return 0;
    }
    let mut declaring: J = 0;
    let derr = jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, m, &mut declaring);
    let csig = if derr == 0 { agent::signature_of(jvmti, declaring) } else { "?".to_string() };
    let mut modifiers: i32 = 0;
    let merr = jvmti!(jvmti, 66, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, m, &mut modifiers);
    let own = if derr == 0 {
        i32::from(jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, declaring, klass) != 0)
    } else {
        -1
    };
    line(format!(
        "  {name}{sig}: found=1 declaring err={derr} {} own={own} modifiers err={merr} 0x{:x}",
        mask(&csig),
        modifiers as u32
    ));
    m
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W60JvmtiLambdaFrames_ids(env: Env, _self: J, lambda: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let klass = jni!(env, 31, extern "system" fn(Env, J) -> J)(env, lambda);
    let run = report(env, jvmti, klass, "run", "()V");
    report(env, jvmti, klass, "<init>", "()V");
    report(env, jvmti, klass, "toString", "()Ljava/lang/String;");
    if run != 0 {
        jni!(env, 63, extern "system" fn(Env, J, J, *const u64))(env, lambda, run, std::ptr::null());
        line(format!("  call: pending={}", exception_check(env)));
        exception_clear(env);
    }
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
