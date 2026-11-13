//! Rust port of tools/probes/interp/L1/L1W61JvmtiEventAndBreakpointErrors.c:
//! the same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CString};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
const CAN_GENERATE_BREAKPOINT_EVENTS: u32 = 1 << 19;

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD_OUT: String = String::new();

type Mode = extern "system" fn(Env, i32, i32, J) -> i32;
type Bp = extern "system" fn(Env, J, i64) -> i32;

unsafe fn mode(jvmti: Env, m: i32, event: i32, thread: J) -> i32 {
    jvmti!(jvmti, 2, Mode)(jvmti, m, event, thread)
}

fn row(out: &mut String, label: &str, err: i32) {
    out.push_str(&format!("{label}: err={err}\n"));
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let mut out = String::new();
    row(&mut out, "onload enable SingleStep without capability", mode(jvmti, 1, 60, 0));
    row(&mut out, "onload mode 7", mode(jvmti, 7, 52, 0));
    row(&mut out, "onload event 49", mode(jvmti, 1, 49, 0));
    row(&mut out, "onload enable ThreadStart", mode(jvmti, 1, 52, 0));
    row(&mut out, "onload disable ThreadStart", mode(jvmti, 0, 52, 0));
    let wanted: [u32; 4] = [CAN_GENERATE_BREAKPOINT_EVENTS, 0, 0, 0];
    let add = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    row(&mut out, "onload AddCapabilities", add);
    row(&mut out, "onload enable Breakpoint", mode(jvmti, 1, 62, 0));
    row(&mut out, "onload disable Breakpoint", mode(jvmti, 0, 62, 0));
    ONLOAD_OUT = out;
    AGENT = jvmti;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiEventAndBreakpointErrors_onload(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    agent::new_string(env, &ONLOAD_OUT)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiEventAndBreakpointErrors_nativeTarget(_env: Env, _self: J) -> i32 {
    1
}

unsafe fn method_id(env: Env, class: J, name: &str, sig: &str, is_static: bool) -> J {
    let (name, sig) = (CString::new(name).unwrap_or_default(), CString::new(sig).unwrap_or_default());
    let n = if is_static { 113 } else { 33 };
    jni!(env, n, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(env, class, name.as_ptr(), sig.as_ptr())
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiEventAndBreakpointErrors_rows(env: Env, this_class: J, unstarted: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut vm: *mut *const usize = std::ptr::null_mut();
    if jni!(env, 219, extern "system" fn(Env, *mut *mut *const usize) -> i32)(env, &mut vm) != 0 {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let Some(bare) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    };
    let mut out = String::new();
    let mut current: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut current);
    let target = method_id(env, this_class, "target", "(I)I", true);
    let native_target = method_id(env, this_class, "nativeTarget", "()I", true);
    let shape_name = CString::new("L1W61JvmtiEventAndBreakpointErrors$Shape").unwrap_or_default();
    let shape = jni!(env, 6, extern "system" fn(Env, *const c_char) -> J)(env, shape_name.as_ptr());
    let abstract_sides = method_id(env, shape, "sides", "()I", false);
    row(&mut out, "mode 7", mode(jvmti, 7, 62, 0));
    row(&mut out, "event 49", mode(jvmti, 1, 49, 0));
    row(&mut out, "event 89", mode(jvmti, 1, 89, 0));
    row(&mut out, "event 72", mode(jvmti, 1, 72, 0));
    row(&mut out, "event 85", mode(jvmti, 1, 85, 0));
    row(&mut out, "VMInit for a thread", mode(jvmti, 1, 50, current));
    row(&mut out, "Breakpoint for a class", mode(jvmti, 1, 62, this_class));
    row(&mut out, "Breakpoint for an unstarted thread", mode(jvmti, 1, 62, unstarted));
    row(&mut out, "SingleStep without capability", mode(jvmti, 1, 60, 0));
    row(&mut out, "disable Breakpoint never enabled", mode(jvmti, 0, 62, 0));
    row(&mut out, "enable Breakpoint for this thread", mode(jvmti, 1, 62, current));
    row(&mut out, "disable Breakpoint for this thread", mode(jvmti, 0, 62, current));
    row(&mut out, "no capability enable Breakpoint", mode(bare, 1, 62, 0));
    let (mut start, mut end): (i64, i64) = (0, 0);
    jvmti!(jvmti, 71, extern "system" fn(Env, J, *mut i64, *mut i64) -> i32)(jvmti, target, &mut start, &mut end);
    out.push_str(&format!("target locations: {start}..{end}\n"));
    let set = jvmti!(jvmti, 38, Bp);
    let clear = jvmti!(jvmti, 39, Bp);
    row(&mut out, "SetBreakpoint NULL method", set(jvmti, 0, 0));
    row(&mut out, "SetBreakpoint location -1", set(jvmti, target, -1));
    row(&mut out, "SetBreakpoint past the end", set(jvmti, target, end + 1));
    row(&mut out, "SetBreakpoint 1000", set(jvmti, target, 1000));
    row(&mut out, "SetBreakpoint native", set(jvmti, native_target, 0));
    row(&mut out, "SetBreakpoint abstract", set(jvmti, abstract_sides, 0));
    row(&mut out, "SetBreakpoint 0", set(jvmti, target, 0));
    row(&mut out, "SetBreakpoint 0 again", set(jvmti, target, 0));
    row(&mut out, "SetBreakpoint end", set(jvmti, target, end));
    row(&mut out, "ClearBreakpoint 0", clear(jvmti, target, 0));
    row(&mut out, "ClearBreakpoint 0 again", clear(jvmti, target, 0));
    row(&mut out, "ClearBreakpoint end", clear(jvmti, target, end));
    row(&mut out, "ClearBreakpoint NULL method", clear(jvmti, 0, 0));
    row(&mut out, "ClearBreakpoint location -1", clear(jvmti, target, -1));
    row(&mut out, "ClearBreakpoint native", clear(jvmti, native_target, 0));
    row(&mut out, "no capability SetBreakpoint", jvmti!(bare, 38, Bp)(bare, target, 0));
    row(&mut out, "no capability ClearBreakpoint", jvmti!(bare, 39, Bp)(bare, target, 0));
    jvmti!(bare, 127, extern "system" fn(Env) -> i32)(bare);
    agent::new_string(env, &out)
}
