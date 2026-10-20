//! Rust port of tools/probes/interp/L1/L1W58JvmtiInNative.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static SPIN_RUNNING: AtomicBool = AtomicBool::new(false);
static SPIN_RELEASED: AtomicBool = AtomicBool::new(false);

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    0
}

unsafe fn state_line(env: Env, label: J, t: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut state: i32 = 0;
    let err = jvmti!(jvmti, 17, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, t, &mut state);
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, label, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, label, chars);
    agent::new_string(env, &format!("{text}: err={err} state=0x{:x}\n", state as u32))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiInNative_state(env: Env, _self: J, label: J, t: J) -> J {
    state_line(env, label, t)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiInNative_stateOfSelf(env: Env, _self: J, label: J) -> J {
    state_line(env, label, 0)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiInNative_spin(_env: Env, _self: J) {
    SPIN_RUNNING.store(true, Ordering::SeqCst);
    while !SPIN_RELEASED.load(Ordering::SeqCst) {
        std::hint::spin_loop();
    }
    SPIN_RUNNING.store(false, Ordering::SeqCst);
    SPIN_RELEASED.store(false, Ordering::SeqCst);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiInNative_spinning(_env: Env, _self: J) -> u8 {
    u8::from(SPIN_RUNNING.load(Ordering::SeqCst))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiInNative_release(_env: Env, _self: J) {
    SPIN_RELEASED.store(true, Ordering::SeqCst);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W58JvmtiInNative_upcall(env: Env, _self: J, r: J) {
    let klass = jni!(env, 31, extern "system" fn(Env, J) -> J)(env, r);
    let run = jni!(env, 33, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        klass,
        c"run".as_ptr(),
        c"()V".as_ptr(),
    );
    if run != 0 {
        jni!(env, 63, extern "system" fn(Env, J, J, *const u64))(env, r, run, std::ptr::null());
    }
}
