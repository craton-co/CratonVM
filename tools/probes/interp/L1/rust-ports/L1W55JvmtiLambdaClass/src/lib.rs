//! Rust port of tools/probes/interp/L1/L1W55JvmtiLambdaClass.c: the same
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

/// The signature with every hex digit after "0x" as 'h'.
fn mask(sig: &str) -> String {
    let Some(at) = sig.find("0x") else {
        return sig.to_string();
    };
    let (head, tail) = sig.split_at(at + 2);
    let digits = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
    format!("{head}{}{}", "h".repeat(digits), &tail[digits..])
}

unsafe fn same(env: Env, a: J, b: J) -> bool {
    jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, a, b) != 0
}

unsafe fn listed(env: Env, count: i32, classes: *mut J, klass: J) -> i32 {
    for i in 0..count.max(0) as usize {
        if same(env, *classes.add(i), klass) {
            return 1;
        }
    }
    0
}

unsafe fn text_of(p: *mut c_char) -> String {
    if p.is_null() {
        "-".into()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
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

type ClassList = extern "system" fn(Env, *mut i32, *mut *mut J) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1W55JvmtiLambdaClass_facts(env: Env, _self: J, klass: J, app: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut count: i32 = 0;
    let mut classes: *mut J = std::ptr::null_mut();
    let err = jvmti!(jvmti, 78, ClassList)(jvmti, &mut count, &mut classes);
    let hit = if err == 0 { listed(env, count, classes, klass) } else { 0 };
    line(format!("loaded classes: err={err} listed={hit}"));
    agent::deallocate(jvmti, classes.cast());
    classes = std::ptr::null_mut();

    let mut sig: *mut c_char = std::ptr::null_mut();
    let mut generic: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 48, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        klass,
        &mut sig,
        &mut generic,
    );
    let masked = if err == 0 { mask(&text_of(sig)) } else { "-".into() };
    line(format!(
        "signature: err={err} sig={masked} generic={}",
        if generic.is_null() { "null" } else { "set" }
    ));
    agent::deallocate(jvmti, sig);
    agent::deallocate(jvmti, generic);

    let mut modifiers: i32 = 0;
    let err = jvmti!(jvmti, 51, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, klass, &mut modifiers);
    line(format!("modifiers: err={err} value=0x{:x}", modifiers as u32));

    let mut loader: J = 0;
    let err = jvmti!(jvmti, 57, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, klass, &mut loader);
    let kind = if err != 0 {
        "-"
    } else if loader == 0 {
        "null"
    } else if same(env, loader, app) {
        "app"
    } else {
        "other"
    };
    line(format!("loader: err={err} kind={kind}"));

    count = 0;
    let err = jvmti!(jvmti, 54, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        klass,
        &mut count,
        &mut classes,
    );
    let first = if err == 0 && count > 0 { agent::signature_of(jvmti, *classes) } else { "-".into() };
    line(format!(
        "interfaces: err={err} count={} first={first}",
        if err == 0 { count } else { -1 }
    ));
    agent::deallocate(jvmti, classes.cast());
    classes = std::ptr::null_mut();

    let mut status: i32 = 0;
    let err = jvmti!(jvmti, 49, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, klass, &mut status);
    line(format!("status: err={err} value={status}"));

    let mut is_interface: u8 = 0;
    let mut is_array: u8 = 0;
    let ierr = jvmti!(jvmti, 55, extern "system" fn(Env, J, *mut u8) -> i32)(jvmti, klass, &mut is_interface);
    let aerr = jvmti!(jvmti, 56, extern "system" fn(Env, J, *mut u8) -> i32)(jvmti, klass, &mut is_array);
    line(format!(
        "kind: interface err={ierr} value={is_interface} array err={aerr} value={is_array}"
    ));

    count = 0;
    let mut methods: *mut J = std::ptr::null_mut();
    let err = jvmti!(jvmti, 52, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        klass,
        &mut count,
        &mut methods,
    );
    line(format!("methods: err={err} count={count}"));
    agent::deallocate(jvmti, methods.cast());

    let mut source: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 50, extern "system" fn(Env, J, *mut *mut c_char) -> i32)(jvmti, klass, &mut source);
    line(format!("source file: err={err}"));
    agent::deallocate(jvmti, source);

    let (mut minor, mut major): (i32, i32) = (0, 0);
    let err = jvmti!(jvmti, 145, extern "system" fn(Env, J, *mut i32, *mut i32) -> i32)(
        jvmti,
        klass,
        &mut minor,
        &mut major,
    );
    line(format!("version: err={err} major={major}"));

    count = 0;
    let err = jvmti!(jvmti, 79, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        app,
        &mut count,
        &mut classes,
    );
    let hit = if err == 0 { listed(env, count, classes, klass) } else { 0 };
    line(format!("app loader classes: err={err} listed={hit}"));
    agent::deallocate(jvmti, classes.cast());

    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
