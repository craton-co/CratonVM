//! Rust port of tools/probes/interp/L1/L1W55JvmtiPhases.c: the same calls
//! in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};
use std::io::Write;

const EVENT_VM_INIT: i32 = 50;
const EVENT_VM_DEATH: i32 = 51;
const EVENT_THREAD_START: i32 = 52;
const EVENT_CLASS_LOAD: i32 = 55;
const EVENT_CLASS_PREPARE: i32 = 56;
const EVENT_VM_START: i32 = 57;
const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static mut OUT: String = String::new();
static mut VMSTART_SEEN: i32 = 0;
static mut VMSTART_PHASE: i32 = 0;
static mut VMINIT_SEEN: i32 = 0;
static mut VMINIT_PHASE: i32 = 0;
static mut VMINIT_AFTER_START: bool = false;
static mut EARLY_LOAD: i32 = 0;
static mut EARLY_PREPARE: i32 = 0;
static mut EARLY_THREAD: i32 = 0;
static mut START_CALLS: [i32; 5] = [-1; 5];

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

unsafe fn phase_of(jvmti: Env) -> i32 {
    let mut phase: i32 = 0;
    jvmti!(jvmti, 133, extern "system" fn(Env, *mut i32) -> i32)(jvmti, &mut phase);
    phase
}

unsafe extern "system" fn on_vm_start(jvmti: Env, env: Env) {
    VMSTART_SEEN += 1;
    VMSTART_PHASE = phase_of(jvmti);
    let mut current: J = 0;
    START_CALLS[0] = jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut current);
    let mut count: i32 = 0;
    let mut threads: *mut J = std::ptr::null_mut();
    START_CALLS[1] = jvmti!(jvmti, 4, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        &mut count,
        &mut threads,
    );
    if START_CALLS[1] == 0 {
        agent::deallocate(jvmti, threads.cast());
    }
    let mut classes: *mut J = std::ptr::null_mut();
    START_CALLS[2] = jvmti!(jvmti, 78, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        &mut count,
        &mut classes,
    );
    if START_CALLS[2] == 0 {
        agent::deallocate(jvmti, classes.cast());
    }
    let object = jni!(env, 6, extern "system" fn(Env, *const c_char) -> J)(
        env,
        b"java/lang/Object\0".as_ptr().cast(),
    );
    if object == 0 {
        jni!(env, 17, extern "system" fn(Env))(env);
    } else {
        let mut sig: *mut c_char = std::ptr::null_mut();
        START_CALLS[3] = jvmti!(jvmti, 48, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char) -> i32)(
            jvmti,
            object,
            &mut sig,
            std::ptr::null_mut(),
        );
        if START_CALLS[3] == 0 {
            agent::deallocate(jvmti, sig);
        }
    }
    START_CALLS[4] = jvmti!(jvmti, 2, extern "system" fn(Env, i32, i32, J) -> i32)(jvmti, 1, EVENT_VM_START, 0);
}

unsafe extern "system" fn on_vm_init(jvmti: Env, _env: Env, _thread: J) {
    VMINIT_SEEN += 1;
    VMINIT_PHASE = phase_of(jvmti);
    VMINIT_AFTER_START = VMSTART_SEEN > 0;
}

unsafe extern "system" fn on_vm_death(jvmti: Env, _env: Env) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "vmdeath: phase={}", phase_of(jvmti));
    let _ = out.flush();
}

unsafe extern "system" fn on_class_load(_jvmti: Env, _env: Env, _thread: J, _klass: J) {
    if VMSTART_SEEN == 0 {
        EARLY_LOAD += 1;
    }
}

unsafe extern "system" fn on_class_prepare(_jvmti: Env, _env: Env, _thread: J, _klass: J) {
    if VMSTART_SEEN == 0 {
        EARLY_PREPARE += 1;
    }
}

unsafe extern "system" fn on_thread_start(_jvmti: Env, _env: Env, _thread: J) {
    if VMSTART_SEEN == 0 {
        EARLY_THREAD += 1;
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    OUT.clear();
    let phase = phase_of(jvmti);
    let set = agent::set_event_callbacks(
        jvmti,
        &[
            (EVENT_VM_START, on_vm_start as *const () as usize),
            (EVENT_VM_INIT, on_vm_init as *const () as usize),
            (EVENT_VM_DEATH, on_vm_death as *const () as usize),
            (EVENT_CLASS_LOAD, on_class_load as *const () as usize),
            (EVENT_CLASS_PREPARE, on_class_prepare as *const () as usize),
            (EVENT_THREAD_START, on_thread_start as *const () as usize),
        ],
    );
    let mode = jvmti!(jvmti, 2, extern "system" fn(Env, i32, i32, J) -> i32);
    let events = [
        EVENT_VM_START,
        EVENT_VM_INIT,
        EVENT_VM_DEATH,
        EVENT_CLASS_LOAD,
        EVENT_CLASS_PREPARE,
        EVENT_THREAD_START,
    ];
    let codes = events.map(|event| mode(jvmti, 1, event, 0));
    line(format!(
        "onload: phase={phase} callbacks={set} start={} init={} death={} load={} prepare={} thread={}",
        codes[0], codes[1], codes[2], codes[3], codes[4], codes[5]
    ));
    let (err, bit, add) = all_class_hooks(jvmti);
    line(format!("onload all class hook events: potential err={err} bit={bit} add={add}"));
    AGENT = jvmti;
    0
}

/// `jvmtiCapabilities`: four words of bit fields.
type Caps = [u32; 4];
/// `can_generate_all_class_hook_events`, the 27th bit field.
const ALL_CLASS_HOOKS: u32 = 1 << 26;

/// `GetPotentialCapabilities`' error and bit, then `AddCapabilities`' error.
unsafe fn all_class_hooks(jvmti: Env) -> (i32, u32, i32) {
    let mut potential: Caps = [0; 4];
    let err = jvmti!(jvmti, 140, extern "system" fn(Env, *mut Caps) -> i32)(jvmti, &mut potential);
    let wanted: Caps = [ALL_CLASS_HOOKS, 0, 0, 0];
    let add = jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &wanted);
    (err, (potential[0] >> 26) & 1, add)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W55JvmtiPhases_livePotential(env: Env, _self: J) -> J {
    let mut vm: *mut *const usize = std::ptr::null_mut();
    let got = jni!(env, 219, extern "system" fn(Env, *mut *mut *const usize) -> i32)(env, &mut vm);
    let other = if got == 0 { agent::get_env(vm, agent::JVMTI_VERSION_1_2) } else { None };
    let Some(other) = other else {
        return agent::new_string(env, "no second env");
    };
    let (err, bit, add) = all_class_hooks(other);
    jvmti!(other, 127, extern "system" fn(Env) -> i32)(other);
    agent::new_string(env, &format!("potential err={err} bit={bit} add={add}"))
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnUnload(_vm: *mut *const usize) {
    if !AGENT.is_null() {
        let mut out = std::io::stdout();
        let _ = writeln!(out, "unload: phase={}", phase_of(AGENT));
        let _ = out.flush();
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W55JvmtiPhases_rows(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let yes = |n: i32| if n > 0 { "yes" } else { "no" };
    line(format!("vmstart: seen={VMSTART_SEEN} phase={VMSTART_PHASE}"));
    line(format!(
        "before vmstart: load={} prepare={} thread={}",
        yes(EARLY_LOAD),
        yes(EARLY_PREPARE),
        yes(EARLY_THREAD)
    ));
    line(format!(
        "start phase calls: current={} threads={} loaded={} signature={} mode={}",
        START_CALLS[0], START_CALLS[1], START_CALLS[2], START_CALLS[3], START_CALLS[4]
    ));
    line(format!(
        "vminit: seen={VMINIT_SEEN} phase={VMINIT_PHASE} after vmstart={}",
        if VMINIT_AFTER_START { "yes" } else { "no" }
    ));
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W55JvmtiPhases_phase(_env: Env, _self: J) -> i32 {
    if AGENT.is_null() {
        -1
    } else {
        phase_of(AGENT)
    }
}
