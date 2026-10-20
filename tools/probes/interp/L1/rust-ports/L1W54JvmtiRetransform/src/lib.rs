//! Rust port of tools/probes/interp/L1/L1W54JvmtiRetransform.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const PREFIX: &str = "L1W54JvmtiRetransform$";
const EVENT_VM_INIT: i32 = 50;
const EVENT_CLASS_FILE_LOAD_HOOK: i32 = 54;
const PHASE_START: i32 = 6;
const PHASE_LIVE: i32 = 4;
const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static mut JAVA_VM: *mut *const usize = std::ptr::null_mut();
static mut OUT: String = String::new();
static mut LIVE: bool = false;
static mut START_BOOT: i64 = 0;
static mut START_OTHER: i64 = 0;
static mut EARLY_PHASE: i64 = 0;

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

fn count_of(data: &[u8], word: &[u8]) -> usize {
    data.windows(6).filter(|w| *w == word).count()
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
    if !LIVE {
        let mut phase: i32 = 0;
        jvmti!(jvmti, 133, extern "system" fn(Env, *mut i32) -> i32)(jvmti, &mut phase);
        if phase == PHASE_START {
            if loader == 0 {
                START_BOOT += 1;
            } else {
                START_OTHER += 1;
            }
        } else if phase != PHASE_LIVE {
            EARLY_PHASE += 1;
        }
        return;
    }
    if name.is_null() {
        return;
    }
    let name = CStr::from_ptr(name).to_string_lossy().into_owned();
    if !name.starts_with(PREFIX) {
        return;
    }
    let data = std::slice::from_raw_parts(class_data, class_data_len as usize);
    let mut rewritten: i32 = -1;
    if class_being_redefined != 0 {
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
    line(format!(
        "hook {name}: redefined={} loader={} domain={} before={} after={} rewritten={rewritten}",
        if class_being_redefined == 0 { "null" } else { "class" },
        loader_kind(env, loader),
        if protection_domain == 0 { "null" } else { "set" },
        count_of(data, b"BEFORE"),
        count_of(data, b"AFTER!"),
    ));
}

unsafe extern "system" fn on_vm_init(_jvmti: Env, _env: Env, _thread: J) {
    LIVE = true;
    let yes = |n: i64| if n > 0 { "yes" } else { "no" };
    line(format!(
        "start phase: boot={} other={} earlier={}",
        yes(START_BOOT),
        yes(START_OTHER),
        yes(EARLY_PHASE)
    ));
}

/// `jvmtiCapabilities`: four words of bit fields.
type Caps = [u32; 4];

fn caps_text(caps: &Caps) -> String {
    format!("retransform={}", (caps[1] >> 5) & 1)
}

unsafe fn potential(jvmti: Env) -> (i32, Caps) {
    let mut caps: Caps = [0; 4];
    let err = jvmti!(jvmti, 140, extern "system" fn(Env, *mut Caps) -> i32)(jvmti, &mut caps);
    (err, caps)
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    OUT.clear();
    let (err, caps) = potential(jvmti);
    line(format!("onload potential: err={err} {}", caps_text(&caps)));
    let wanted: Caps = [0, 1 << 5, 0, 0];
    let added = jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &wanted);
    line(format!("add can_retransform_classes: {added}"));
    let callbacks = agent::set_event_callbacks(
        jvmti,
        &[
            (EVENT_CLASS_FILE_LOAD_HOOK, on_class_file_load_hook as *const () as usize),
            (EVENT_VM_INIT, on_vm_init as *const () as usize),
        ],
    );
    let mode = jvmti!(jvmti, 2, extern "system" fn(Env, i32, i32, J) -> i32);
    let cflh = mode(jvmti, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0);
    let init = mode(jvmti, 1, EVENT_VM_INIT, 0);
    line(format!("onload: callbacks={callbacks} cflh={cflh} init={init}"));
    AGENT = jvmti;
    JAVA_VM = vm;
    0
}

type Retransform = extern "system" fn(Env, i32, *const J) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_retransform(env: Env, _self: J, classes: J, count: i32) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let length = jni!(env, 171, extern "system" fn(Env, J) -> i32)(env, classes);
    let mut list = [0 as J; 8];
    for (i, slot) in list.iter_mut().enumerate().take(length.clamp(0, 8) as usize) {
        *slot = jni!(env, 173, extern "system" fn(Env, J, i32) -> J)(env, classes, i as i32);
    }
    let err = jvmti!(AGENT, 152, Retransform)(AGENT, count, list.as_ptr());
    agent::new_string(env, &format!("err={err}"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_retransformNull(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let err = jvmti!(AGENT, 152, Retransform)(AGENT, 1, std::ptr::null());
    agent::new_string(env, &format!("err={err}"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_withoutCapability(env: Env, _self: J, klass: J) -> J {
    let other = if JAVA_VM.is_null() {
        None
    } else {
        agent::get_env(JAVA_VM, agent::JVMTI_VERSION_1_2)
    };
    let Some(other) = other else {
        return agent::new_string(env, "no second env");
    };
    let err = jvmti!(other, 152, Retransform)(other, 1, &klass);
    jvmti!(other, 127, extern "system" fn(Env) -> i32)(other);
    agent::new_string(env, &format!("err={err}"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_livePotential(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let (err, caps) = potential(AGENT);
    agent::new_string(env, &format!("err={err} {}", caps_text(&caps)))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_loaderClasses(env: Env, _self: J, loader: J, queries: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let mut count: i32 = 0;
    let mut classes: *mut J = std::ptr::null_mut();
    let err = jvmti!(AGENT, 79, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(
        AGENT,
        loader,
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
            "err={err} probe={} target={} target[]={} string={}",
            found[0], found[1], found[2], found[3]
        ),
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_modifiable(env: Env, _self: J, queries: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let mut answers = [0i32; 5];
    for (q, answer) in answers.iter_mut().enumerate() {
        let klass = jni!(env, 173, extern "system" fn(Env, J, i32) -> J)(env, queries, q as i32);
        let mut modifiable: u8 = 0;
        let err = jvmti!(AGENT, 45, extern "system" fn(Env, J, *mut u8) -> i32)(AGENT, klass, &mut modifiable);
        *answer = if err != 0 { -err } else { i32::from(modifiable) };
    }
    agent::new_string(
        env,
        &format!(
            "target={} int={} int[]={} string={} lambda={}",
            answers[0], answers[1], answers[2], answers[3], answers[4]
        ),
    )
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_lambdaFacts(env: Env, _self: J, lambda: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let mut count: i32 = 0;
    let mut classes: *mut J = std::ptr::null_mut();
    let err = jvmti!(AGENT, 78, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(
        AGENT,
        &mut count,
        &mut classes,
    );
    let mut loaded = 0;
    if err == 0 {
        for i in 0..count as usize {
            if jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, *classes.add(i), lambda) != 0 {
                loaded = 1;
            }
        }
    }
    agent::deallocate(AGENT, classes.cast());
    let mut sig: *mut c_char = std::ptr::null_mut();
    let sig_err = jvmti!(AGENT, 48, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char) -> i32)(
        AGENT,
        lambda,
        &mut sig,
        std::ptr::null_mut(),
    );
    agent::deallocate(AGENT, sig);
    agent::new_string(env, &format!("loaded={loaded} signature-err={sig_err}"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W54JvmtiRetransform_rows(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
