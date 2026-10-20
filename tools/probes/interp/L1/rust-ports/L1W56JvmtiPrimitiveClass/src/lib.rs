//! Rust port of tools/probes/interp/L1/L1W56JvmtiPrimitiveClass.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_get_source_file_name`, the 12th bit field.
const CAN_GET_SOURCE_FILE_NAME: u32 = 1 << 11;

static mut AGENT: Env = std::ptr::null_mut();
static mut OUT: String = String::new();

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [CAN_GET_SOURCE_FILE_NAME, 0, 0, 0];
    OUT.clear();
    let add = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    line(format!("onload: add={add}"));
    AGENT = jvmti;
    0
}

type IntOf = extern "system" fn(Env, J, *mut i32) -> i32;
type BoolOf = extern "system" fn(Env, J, *mut u8) -> i32;
type ListOf = extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1W56JvmtiPrimitiveClass_facts(env: Env, _self: J, klass: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut sig: *mut c_char = std::ptr::null_mut();
    let mut generic: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 48, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        klass,
        &mut sig,
        &mut generic,
    );
    let shown = if err == 0 && !sig.is_null() { CStr::from_ptr(sig).to_string_lossy().into_owned() } else { "-".into() };
    line(format!(
        "signature: err={err} sig={shown} generic={}",
        if generic.is_null() { "null" } else { "set" }
    ));
    agent::deallocate(jvmti, sig);
    agent::deallocate(jvmti, generic);
    let mut value: i32 = -1;
    let err = jvmti!(jvmti, 49, IntOf)(jvmti, klass, &mut value);
    line(format!("status: err={err} value={value}"));
    value = -1;
    let err = jvmti!(jvmti, 51, IntOf)(jvmti, klass, &mut value);
    line(format!("modifiers: err={err} value=0x{:x}", value as u32));
    for (slot, label) in [(52, "methods"), (53, "fields"), (54, "interfaces")] {
        let mut count: i32 = -1;
        let mut list: *mut J = std::ptr::null_mut();
        let err = jvmti!(jvmti, slot, ListOf)(jvmti, klass, &mut count, &mut list);
        line(format!("{label}: err={err} count={count}"));
        agent::deallocate(jvmti, list.cast());
    }
    let mut flag: u8 = 2;
    let err = jvmti!(jvmti, 55, BoolOf)(jvmti, klass, &mut flag);
    line(format!("interface: err={err} value={flag}"));
    flag = 2;
    let err = jvmti!(jvmti, 56, BoolOf)(jvmti, klass, &mut flag);
    line(format!("array: err={err} value={flag}"));
    let mut loader: J = 1;
    let err = jvmti!(jvmti, 57, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, klass, &mut loader);
    line(format!("loader: err={err} null={}", u8::from(loader == 0)));
    let mut source: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 50, extern "system" fn(Env, J, *mut *mut c_char) -> i32)(jvmti, klass, &mut source);
    line(format!("source file: err={err}"));
    agent::deallocate(jvmti, source);
    let (mut minor, mut major): (i32, i32) = (-1, -1);
    let err = jvmti!(jvmti, 145, extern "system" fn(Env, J, *mut i32, *mut i32) -> i32)(
        jvmti,
        klass,
        &mut minor,
        &mut major,
    );
    line(format!("version: err={err}"));
    let mut modifiable: u8 = 2;
    let err = jvmti!(jvmti, 45, BoolOf)(jvmti, klass, &mut modifiable);
    line(format!("modifiable: err={err} value={modifiable}"));
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
