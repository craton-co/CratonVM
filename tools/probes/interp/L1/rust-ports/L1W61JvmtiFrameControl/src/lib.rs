//! Rust port of tools/probes/interp/L1/L1W61JvmtiFrameControl.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
const CAN_POP_FRAME: u32 = 1 << 8;
const CAN_GENERATE_FRAME_POP_EVENTS: u32 = 1 << 18;
const CAN_SUSPEND: u32 = 1 << 20;
/// Word 1.
const CAN_FORCE_EARLY_RETURN: u32 = 1 << 1;

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD_POTENTIAL: Caps = [0; 4];
static mut ONLOAD_ADD: i32 = -1;
static mut ONLOAD_ENABLE: i32 = -1;
static mut POPS: String = String::new();

type Caps = [u32; 4];
type ThreadFn = extern "system" fn(Env, J) -> i32;

unsafe fn name_of(jvmti: Env, method: J) -> String {
    let mut name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        method,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if err != 0 || name.is_null() {
        return "?".into();
    }
    agent::take(jvmti, name)
}

unsafe extern "system" fn on_frame_pop(jvmti: Env, _env: Env, _thread: J, method: J, popped: u8) {
    let item = format!("  FramePop {} popped={}\n", name_of(jvmti, method), popped);
    POPS.push_str(&item);
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    jvmti!(jvmti, 140, extern "system" fn(Env, *mut Caps) -> i32)(jvmti, &mut ONLOAD_POTENTIAL);
    let wanted: Caps = [CAN_SUSPEND | CAN_POP_FRAME | CAN_GENERATE_FRAME_POP_EVENTS, CAN_FORCE_EARLY_RETURN, 0, 0];
    ONLOAD_ADD = jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(jvmti, &[(61, on_frame_pop as usize)]);
    ONLOAD_ENABLE = jvmti!(jvmti, 2, extern "system" fn(Env, i32, i32, J) -> i32)(jvmti, 1, 61, 0);
    AGENT = jvmti;
    0
}

fn bit(caps: &Caps, word: usize, bit: u32) -> i32 {
    i32::from(caps[word] & bit != 0)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_caps(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut vm: *mut *const usize = std::ptr::null_mut();
    if jni!(env, 219, extern "system" fn(Env, *mut *mut *const usize) -> i32)(env, &mut vm) != 0 {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let Some(bare) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    };
    let mut potential: Caps = [0; 4];
    jvmti!(bare, 140, extern "system" fn(Env, *mut Caps) -> i32)(bare, &mut potential);
    let pop = jvmti!(bare, 80, ThreadFn)(bare, 0);
    let early = jvmti!(bare, 82, extern "system" fn(Env, J, i32) -> i32)(bare, 0, 1);
    let clear = jvmti!(bare, 67, ThreadFn)(bare, 0);
    let add = jvmti!(bare, 142, extern "system" fn(Env, *const Caps) -> i32);
    let add_pop = add(bare, &[CAN_POP_FRAME, 0, 0, 0]);
    let add_early = add(bare, &[0, CAN_FORCE_EARLY_RETURN, 0, 0]);
    jvmti!(bare, 127, extern "system" fn(Env) -> i32)(bare);
    let out = format!(
        "caps onload: pop={} early={} add={} enable={}\ncaps live: pop={} early={}\ncaps none: pop={pop} early={early} clear={clear}\ncaps live add: pop={add_pop} early={add_early}\n",
        bit(&ONLOAD_POTENTIAL, 0, CAN_POP_FRAME),
        bit(&ONLOAD_POTENTIAL, 1, CAN_FORCE_EARLY_RETURN),
        ONLOAD_ADD,
        ONLOAD_ENABLE,
        bit(&potential, 0, CAN_POP_FRAME),
        bit(&potential, 1, CAN_FORCE_EARLY_RETURN),
    );
    agent::new_string(env, &out)
}

unsafe fn thread_call(n: usize, t: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, n, ThreadFn)(AGENT, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_suspend(_env: Env, _self: J, t: J) -> i32 {
    thread_call(5, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_resume(_env: Env, _self: J, t: J) -> i32 {
    thread_call(6, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_popFrame(_env: Env, _self: J, t: J) -> i32 {
    thread_call(80, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_earlyVoid(_env: Env, _self: J, t: J) -> i32 {
    thread_call(86, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_clearAllFramePops(_env: Env, _self: J, t: J) -> i32 {
    thread_call(67, t)
}

unsafe fn frame_count(t: J) -> Result<i32, i32> {
    let mut count: i32 = 0;
    let err = jvmti!(AGENT, 16, extern "system" fn(Env, J, *mut i32) -> i32)(AGENT, t, &mut count);
    if err == 0 {
        Ok(count)
    } else {
        Err(err)
    }
}

unsafe fn frame_method(t: J, depth: i32) -> Result<J, i32> {
    let mut method: J = 0;
    let mut location: i64 = 0;
    let err = jvmti!(AGENT, 19, extern "system" fn(Env, J, i32, *mut J, *mut i64) -> i32)(
        AGENT,
        t,
        depth,
        &mut method,
        &mut location,
    );
    if err == 0 {
        Ok(method)
    } else {
        Err(err)
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_frameCount(_env: Env, _self: J, t: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    match frame_count(t) {
        Ok(count) => count,
        Err(err) => -err,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_frameName(env: Env, _self: J, t: J, depth: i32) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    match frame_method(t, depth) {
        Ok(method) => agent::new_string(env, &name_of(AGENT, method)),
        Err(err) => agent::new_string(env, &format!("err={err}")),
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_depthOf(env: Env, _self: J, t: J, name: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    let Ok(count) = frame_count(t) else {
        return -1;
    };
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, name, std::ptr::null_mut());
    let want = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, name, chars);
    for depth in 0..count {
        if let Ok(method) = frame_method(t, depth) {
            if name_of(AGENT, method) == want {
                return depth;
            }
        }
    }
    -1
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_earlyInt(_env: Env, _self: J, t: J, v: i32) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 82, extern "system" fn(Env, J, i32) -> i32)(AGENT, t, v)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_earlyLong(_env: Env, _self: J, t: J, v: i64) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 83, extern "system" fn(Env, J, i64) -> i32)(AGENT, t, v)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_earlyFloat(_env: Env, _self: J, t: J, v: f32) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 84, extern "system" fn(Env, J, f32) -> i32)(AGENT, t, v)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_earlyDouble(_env: Env, _self: J, t: J, v: f64) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 85, extern "system" fn(Env, J, f64) -> i32)(AGENT, t, v)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_earlyObject(_env: Env, _self: J, t: J, v: J) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 81, extern "system" fn(Env, J, J) -> i32)(AGENT, t, v)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_notifyFramePop(_env: Env, _self: J, t: J, depth: i32) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    jvmti!(AGENT, 20, extern "system" fn(Env, J, i32) -> i32)(AGENT, t, depth)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiFrameControl_events(env: Env, _self: J) -> J {
    let out = agent::new_string(env, &POPS);
    POPS.clear();
    out
}
