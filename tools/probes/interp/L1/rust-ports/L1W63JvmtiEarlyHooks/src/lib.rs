//! Rust port of tools/probes/interp/L1/L1W63JvmtiEarlyHooks.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

static mut ONLOAD: String = String::new();
static mut START_PHASE: i32 = -1;
static mut STARTED: bool = false;
static mut HOOKS_BEFORE_START: i32 = 0;
static mut FIRST_HOOK_PHASE: i32 = -1;
static mut OBJECT_BEFORE_START: bool = false;
static mut INIT_SEEN: bool = false;

unsafe fn phase(jvmti: Env) -> i32 {
    let mut p: i32 = -1;
    jvmti!(jvmti, 133, extern "system" fn(Env, *mut i32) -> i32)(jvmti, &mut p);
    p
}

unsafe extern "system" fn on_vm_start(jvmti: Env, _jni: Env) {
    START_PHASE = phase(jvmti);
    STARTED = true;
}

unsafe extern "system" fn on_vm_init(_jvmti: Env, _jni: Env, _t: J) {
    INIT_SEEN = true;
}

unsafe extern "system" fn on_hook(
    jvmti: Env,
    _jni: Env,
    _redefined: J,
    _loader: J,
    name: *const c_char,
    _domain: J,
    _len: i32,
    _data: *const u8,
    _new_len: *mut i32,
    _new_data: *mut *mut u8,
) {
    if FIRST_HOOK_PHASE < 0 {
        FIRST_HOOK_PHASE = phase(jvmti);
    }
    if !STARTED {
        HOOKS_BEFORE_START += 1;
        if !name.is_null() && CStr::from_ptr(name).to_bytes() == b"java/lang/Object" {
            OBJECT_BEFORE_START = true;
        }
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _o: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let mut potential: [u32; 4] = [0; 4];
    jvmti!(jvmti, 140, extern "system" fn(Env, *mut [u32; 4]) -> i32)(jvmti, &mut potential);
    let early_vmstart = (potential[1] >> 9) & 1;
    let early_hook = (potential[1] >> 10) & 1;
    // can_generate_all_class_hook_events (word 0, bit 26) and the two early
    // ones (word 1, bits 9 and 10).
    let wanted: [u32; 4] = [1 << 26, (1 << 9) | (1 << 10), 0, 0];
    let e = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    ONLOAD = format!("onload: potential early_vmstart={early_vmstart} early_class_hook={early_hook} add={e}\n");
    agent::set_event_callbacks(
        jvmti,
        &[
            (57, on_vm_start as *const () as usize),
            (50, on_vm_init as *const () as usize),
            (54, on_hook as *const () as usize),
        ],
    );
    for event in [57, 50, 54] {
        jvmti!(jvmti, 2, extern "system" fn(Env, i32, i32, J) -> i32)(jvmti, 1, event, 0);
    }
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiEarlyHooks_rows(env: Env, _cls: J) -> J {
    agent::new_string(
        env,
        &format!(
            "{ONLOAD}vmstart: seen={} phase={START_PHASE}\nhook: first phase={FIRST_HOOK_PHASE} before vmstart={} object before vmstart={}\nvminit: seen={}\n",
            u8::from(STARTED),
            u8::from(HOOKS_BEFORE_START > 0),
            u8::from(OBJECT_BEFORE_START),
            u8::from(INIT_SEEN)
        ),
    )
}
