//! Rust port of tools/probes/interp/L1/L1W53JvmtiClassHooks.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const PREFIX: &str = "L1W53JvmtiClassHooks$";
const EVENT_CLASS_FILE_LOAD_HOOK: i32 = 54;
const EVENT_CLASS_LOAD: i32 = 55;

static mut AGENT: Env = std::ptr::null_mut();
static mut JAVA_VM: *mut *const usize = std::ptr::null_mut();
static mut OUT: String = String::new();

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

unsafe fn loader_kind(env: Env, loader: J) -> &'static str {
    if loader == 0 {
        return "null";
    }
    let class = jni!(env, 6, extern "system" fn(Env, *const c_char) -> J)(
        env,
        b"java/lang/ClassLoader\0".as_ptr().cast(),
    );
    if class == 0 {
        jni!(env, 17, extern "system" fn(Env))(env);
        return "?";
    }
    let method = jni!(env, 113, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        class,
        b"getSystemClassLoader\0".as_ptr().cast(),
        b"()Ljava/lang/ClassLoader;\0".as_ptr().cast(),
    );
    if method == 0 {
        jni!(env, 17, extern "system" fn(Env))(env);
        return "?";
    }
    let app = jni!(env, 116, extern "system" fn(Env, J, J, *const u64) -> J)(
        env,
        class,
        method,
        std::ptr::null(),
    );
    if jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, loader, app) != 0 {
        "app"
    } else {
        "other"
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn on_class_file_load_hook(
    jvmti: Env,
    env: Env,
    class_being_redefined: J,
    loader: J,
    name: *const c_char,
    protection_domain: J,
    class_data_len: i32,
    class_data: *const u8,
    new_class_data_len: *mut i32,
    new_class_data: *mut *mut u8,
) {
    if name.is_null() {
        return;
    }
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    if !name.starts_with(PREFIX) {
        return;
    }
    let mut phase: i32 = 0;
    jvmti!(jvmti, 133, extern "system" fn(Env, *mut i32) -> i32)(jvmti, &mut phase);
    let data = std::slice::from_raw_parts(class_data, class_data_len as usize);
    let magic = data.len() > 4 && data[..4] == [0xCA, 0xFE, 0xBA, 0xBE];
    let mut rewritten: i32 = -1;
    if name == format!("{PREFIX}Patched") {
        rewritten = 0;
        let mut copy: *mut u8 = std::ptr::null_mut();
        if jvmti!(jvmti, 46, extern "system" fn(Env, i64, *mut *mut u8) -> i32)(
            jvmti,
            i64::from(class_data_len),
            &mut copy,
        ) == 0
        {
            let out = std::slice::from_raw_parts_mut(copy, data.len());
            out.copy_from_slice(data);
            let mut i = 0;
            while i + 6 <= out.len() {
                if &out[i..i + 6] == b"BEFORE" {
                    out[i..i + 6].copy_from_slice(b"AFTER!");
                    rewritten += 1;
                }
                i += 1;
            }
            *new_class_data_len = class_data_len;
            *new_class_data = copy;
        }
    }
    let head = format!(
        "hook {name}: redefined={} loader={} domain={} magic={} phase={phase}",
        if class_being_redefined == 0 { "null" } else { "class" },
        loader_kind(env, loader),
        if protection_domain == 0 { "null" } else { "set" },
        if magic { "ok" } else { "bad" },
    );
    if rewritten < 0 {
        line(head);
    } else {
        line(format!("{head} rewritten={rewritten}"));
    }
}

unsafe extern "system" fn on_class_load(jvmti: Env, env: Env, thread: J, klass: J) {
    let sig = agent::signature_of(jvmti, klass);
    if !sig.contains(PREFIX) {
        return;
    }
    let mut status: i32 = -1;
    jvmti!(jvmti, 49, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, klass, &mut status);
    let mut current: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut current);
    let mut loader: J = 0;
    jvmti!(jvmti, 57, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, klass, &mut loader);
    let same = jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, thread, current) != 0;
    line(format!(
        "load {sig}: status={status} thread={} loader={}",
        if same { "current" } else { "other" },
        loader_kind(env, loader)
    ));
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, options: *mut c_char, _r: *mut c_void) -> i32 {
    if !options.is_null() && CStr::from_ptr(options).to_bytes() == b"fail" {
        // interpreter-L1-a-failed-startup-agent-panics-instead-of-hotspots-init-error-FIXED-20261018.
        return 7;
    }
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    OUT.clear();
    let err = agent::set_event_callbacks(
        jvmti,
        &[
            (EVENT_CLASS_FILE_LOAD_HOOK, on_class_file_load_hook as *const () as usize),
            (EVENT_CLASS_LOAD, on_class_load as *const () as usize),
        ],
    );
    let options = if options.is_null() {
        "NULL".to_string()
    } else {
        CStr::from_ptr(options).to_string_lossy().into_owned()
    };
    line(format!("onload: callbacks err={err} options={options}"));
    AGENT = jvmti;
    JAVA_VM = vm;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W53JvmtiClassHooks_setHooks(env: Env, _self: J, on: u8) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    let mode = jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32);
    let flag = i32::from(on != 0);
    let cflh = mode(AGENT, flag, EVENT_CLASS_FILE_LOAD_HOOK, 0);
    let load = mode(AGENT, flag, EVENT_CLASS_LOAD, 0);
    agent::new_string(env, &format!("cflh={cflh} load={load}"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W53JvmtiClassHooks_loadedClasses(env: Env, _self: J, queries: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    let mut count: i32 = 0;
    let mut classes: *mut J = std::ptr::null_mut();
    let err = jvmti!(AGENT, 78, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(
        AGENT,
        &mut count,
        &mut classes,
    );
    let mut found = [0i32; 4];
    if err == 0 {
        for (q, hit) in found.iter_mut().enumerate() {
            let query = jni!(env, 173, extern "system" fn(Env, J, i32) -> J)(env, queries, q as i32);
            for i in 0..count as usize {
                if jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, *classes.add(i), query) != 0 {
                    *hit = 1;
                }
            }
        }
    }
    agent::deallocate(AGENT, classes.cast());
    agent::new_string(
        env,
        &format!(
            "err={err} probe={} target={} int[]={} int={}",
            found[0], found[1], found[2], found[3]
        ),
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W53JvmtiClassHooks_envVersions(env: Env, _self: J) -> J {
    let versions = [
        0x3009_0000, 0x3009_0100, 0x300A_0000, 0x300B_0100, 0x300C_0000, 0x300D_0500, 0x301A_0000,
    ];
    let mut codes = [1i32; 7];
    if !JAVA_VM.is_null() {
        let get_env: extern "system" fn(*mut *const usize, *mut Env, i32) -> i32 =
            std::mem::transmute(*(*JAVA_VM).add(6));
        for (code, version) in codes.iter_mut().zip(versions) {
            let mut other: Env = std::ptr::null_mut();
            *code = get_env(JAVA_VM, &mut other, version);
        }
    }
    agent::new_string(
        env,
        &format!(
            "9.0={} 9.1={} 10.0={} 11.1={} 12.0={} 13.5={} 26.0={}",
            codes[0], codes[1], codes[2], codes[3], codes[4], codes[5], codes[6]
        ),
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W53JvmtiClassHooks_perThread(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, "no JVMTI env (load the shim with -agentpath too)");
    }
    let mut thread: J = 0;
    jvmti!(AGENT, 18, extern "system" fn(Env, *mut J) -> i32)(AGENT, &mut thread);
    let mode = jvmti!(AGENT, 2, extern "system" fn(Env, i32, i32, J) -> i32);
    let mut codes = [0i32; 5];
    for (code, event) in codes.iter_mut().zip([54, 55, 52, 53, 56]) {
        *code = mode(AGENT, 1, event, thread);
        mode(AGENT, 0, event, thread);
    }
    agent::new_string(
        env,
        &format!(
            "cflh={} load={} start={} end={} prepare={}",
            codes[0], codes[1], codes[2], codes[3], codes[4]
        ),
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W53JvmtiClassHooks_rows(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, "no JVMTI env (load the shim with -agentpath too)\n");
    }
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
