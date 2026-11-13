//! Rust port of tools/probes/interp/L1/L1W60JvmtiLambdaMethodEvents.c: the
//! same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_generate_method_entry_events` (bit 24) and
/// `can_generate_method_exit_events` (bit 25).
const WANTED: u32 = (1 << 24) | (1 << 25);
const METHOD_ENTRY: i32 = 65;
const METHOD_EXIT: i32 = 66;

static mut AGENT: Env = std::ptr::null_mut();
static mut PROXY_ENTRIES: i32 = 0;
static mut PROXY_EXITS: i32 = 0;
static mut BODY_ENTRIES: i32 = 0;
static mut BODY_EXITS: i32 = 0;
static mut NAMES: String = String::new();

type Names = extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;

/// Counts one event of `method`: a method of a lambda's class or a lambda
/// body.
unsafe fn count(jvmti: Env, method: J, entry: bool) {
    let mut declaring: J = 0;
    jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, method, &mut declaring);
    let csig = agent::signature_of(jvmti, declaring);
    let mut name: *mut c_char = std::ptr::null_mut();
    jvmti!(jvmti, 64, Names)(jvmti, method, &mut name, std::ptr::null_mut(), std::ptr::null_mut());
    let name = if name.is_null() { String::new() } else { agent::take(jvmti, name) };
    if csig.contains("$$Lambda") {
        if entry {
            PROXY_ENTRIES += 1;
            if NAMES.len() + name.len() + 2 < 512 {
                NAMES.push(' ');
                NAMES.push_str(&name);
            }
        } else {
            PROXY_EXITS += 1;
        }
    } else if name.starts_with("lambda$main$") {
        if entry {
            BODY_ENTRIES += 1;
        } else {
            BODY_EXITS += 1;
        }
    }
}

unsafe extern "system" fn on_entry(jvmti: Env, _env: Env, _thread: J, method: J) {
    count(jvmti, method, true);
}

unsafe extern "system" fn on_exit(jvmti: Env, _env: Env, _thread: J, method: J, _popped: u8, _value: u64) {
    count(jvmti, method, false);
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [WANTED, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(
        jvmti,
        &[(METHOD_ENTRY, on_entry as usize), (METHOD_EXIT, on_exit as usize)],
    );
    AGENT = jvmti;
    0
}

unsafe fn current_thread(jvmti: Env) -> J {
    let mut thread: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut thread);
    thread
}

unsafe fn notify(jvmti: Env, mode: i32, event: i32, thread: J) {
    jvmti!(jvmti, 2, extern "C" fn(Env, i32, i32, J) -> i32)(jvmti, mode, event, thread);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W60JvmtiLambdaMethodEvents_start(_env: Env, _self: J) {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return;
    }
    PROXY_ENTRIES = 0;
    PROXY_EXITS = 0;
    BODY_ENTRIES = 0;
    BODY_EXITS = 0;
    NAMES.clear();
    let thread = current_thread(jvmti);
    notify(jvmti, 1, METHOD_ENTRY, thread);
    notify(jvmti, 1, METHOD_EXIT, thread);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W60JvmtiLambdaMethodEvents_stop(env: Env, _self: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let thread = current_thread(jvmti);
    notify(jvmti, 0, METHOD_ENTRY, thread);
    notify(jvmti, 0, METHOD_EXIT, thread);
    let out = format!(
        "events: proxy entry={PROXY_ENTRIES} exit={PROXY_EXITS} body entry={BODY_ENTRIES} exit={BODY_EXITS} names=[{}]\n",
        NAMES
    );
    agent::new_string(env, &out)
}
