//! Rust port of tools/probes/interp/L6/L6K54BlockedSyncNativeAndEntryOrder.c:
//! the same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

/// `can_get_owned_monitor_info` (bit 5) and
/// `can_generate_method_entry_events` (bit 24).
const WANTED: u32 = (1 << 5) | (1 << 24);
const METHOD_ENTRY: i32 = 65;

static mut AGENT: Env = std::ptr::null_mut();
static mut ENTRY_LINE: String = String::new();
static mut ENTRIES: i32 = 0;

type Names = extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;

unsafe fn name_of(jvmti: Env, method: J) -> Option<String> {
    let mut name: *mut c_char = std::ptr::null_mut();
    if jvmti!(jvmti, 64, Names)(jvmti, method, &mut name, std::ptr::null_mut(), std::ptr::null_mut()) != 0
        || name.is_null()
    {
        return None;
    }
    Some(agent::take(jvmti, name))
}

unsafe extern "system" fn on_entry(jvmti: Env, _env: Env, thread: J, method: J) {
    let Some(name) = name_of(jvmti, method) else {
        return;
    };
    if name == "contended" {
        ENTRIES += 1;
    }
    if name == "contended" && ENTRY_LINE.is_empty() {
        let mut count: i32 = -1;
        let mut monitors: *mut c_char = std::ptr::null_mut();
        let err = jvmti!(jvmti, 10, extern "system" fn(Env, J, *mut i32, *mut *mut c_char) -> i32)(
            jvmti,
            thread,
            &mut count,
            &mut monitors,
        );
        ENTRY_LINE = format!("contended err={err} owned={count}");
        agent::deallocate(jvmti, monitors);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [WANTED, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(jvmti, &[(METHOD_ENTRY, on_entry as *const () as usize)]);
    AGENT = jvmti;
    0
}

unsafe fn notify(jvmti: Env, mode: i32, event: i32, thread: J) {
    jvmti!(jvmti, 2, extern "C" fn(Env, i32, i32, J) -> i32)(jvmti, mode, event, thread);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L6K54BlockedSyncNativeAndEntryOrder_arm(_env: Env, _cls: J, t: J) {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return;
    }
    ENTRY_LINE.clear();
    ENTRIES = 0;
    notify(jvmti, 1, METHOD_ENTRY, t);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L6K54BlockedSyncNativeAndEntryOrder_disarm(env: Env, _cls: J, t: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, "no JVMTI env");
    }
    notify(jvmti, 0, METHOD_ENTRY, t);
    if ENTRY_LINE.is_empty() {
        agent::new_string(env, "none")
    } else {
        agent::new_string(env, &format!("{ENTRY_LINE} entries={ENTRIES}"))
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L6K54BlockedSyncNativeAndEntryOrder_top(env: Env, _cls: J, t: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, "no JVMTI env");
    }
    // `jvmtiFrameInfo`: a `jmethodID` and a `jlocation`.
    let mut frames = [(0usize, 0i64); 2];
    let mut count: i32 = 0;
    let err = jvmti!(jvmti, 104, extern "system" fn(Env, J, i32, i32, *mut (usize, i64), *mut i32) -> i32)(
        jvmti,
        t,
        0,
        2,
        frames.as_mut_ptr(),
        &mut count,
    );
    let mut out = format!("err={err}");
    if err == 0 {
        for (i, &(method, location)) in frames.iter().take(count.max(0) as usize).enumerate() {
            let name = name_of(jvmti, method).unwrap_or_else(|| "?".into());
            out.push_str(if i == 0 { " " } else { " < " });
            out.push_str(&format!("{name}@{location}"));
        }
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L6K54BlockedSyncNativeAndEntryOrder_staticNative(_env: Env, _cls: J) {}

#[no_mangle]
pub unsafe extern "system" fn Java_L6K54BlockedSyncNativeAndEntryOrder_instanceNative(_env: Env, _self: J) {}
