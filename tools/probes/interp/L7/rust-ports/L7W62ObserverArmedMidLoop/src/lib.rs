//! Rust port of tools/probes/interp/L7/L7W62ObserverArmedMidLoop.c: the
//! same calls in the same order, the same counts. Built like the ports of
//! tools/probes/interp/L1/rust-ports/README.md, whose `common/agent.rs` it
//! includes.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../../../L1/rust-ports/common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI32, Ordering};

const CAN_GET_LINE_NUMBERS: u32 = 1 << 12;
const CAN_GENERATE_SINGLE_STEP_EVENTS: u32 = 1 << 16;
const CAN_GENERATE_BREAKPOINT_EVENTS: u32 = 1 << 19;
const CAN_GENERATE_METHOD_ENTRY_EVENTS: u32 = 1 << 24;

const EVENT_SINGLE_STEP: i32 = 60;
const EVENT_BREAKPOINT: i32 = 62;
const EVENT_METHOD_ENTRY: i32 = 65;

/// The probe's `count` indices: SPIN_STEP, TICK, SPIN_BREAK, SPIN_SELF.
const KNOWN: [&str; 4] = ["spinStep", "tick", "spinBreak", "spinSelf"];
static COUNTS: [AtomicI32; 4] = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];

static mut AGENT: Env = std::ptr::null_mut();

/// `jvmtiLineNumberEntry`.
#[repr(C)]
struct LineEntry {
    start_location: i64,
    line_number: i32,
}

unsafe fn note(jvmti: Env, method: J) {
    let mut name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        method,
        &mut name,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    );
    if err != 0 || name.is_null() {
        return;
    }
    let name = agent::take(jvmti, name);
    if let Some(i) = KNOWN.iter().position(|k| *k == name) {
        COUNTS[i].fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "system" fn on_single_step(jvmti: Env, _env: Env, _thread: J, method: J, _location: i64) {
    note(jvmti, method);
}

unsafe extern "system" fn on_breakpoint(jvmti: Env, _env: Env, _thread: J, method: J, _location: i64) {
    note(jvmti, method);
}

unsafe extern "system" fn on_method_entry(jvmti: Env, _env: Env, _thread: J, method: J) {
    note(jvmti, method);
}

unsafe fn set_mode(jvmti: Env, enable: bool, event: i32, thread: J) -> i32 {
    jvmti!(jvmti, 2, extern "system" fn(Env, i32, i32, J) -> i32)(jvmti, if enable { 1 } else { 0 }, event, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [
        CAN_GENERATE_SINGLE_STEP_EVENTS
            | CAN_GENERATE_METHOD_ENTRY_EVENTS
            | CAN_GENERATE_BREAKPOINT_EVENTS
            | CAN_GET_LINE_NUMBERS,
        0,
        0,
        0,
    ];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    agent::set_event_callbacks(
        jvmti,
        &[
            (EVENT_SINGLE_STEP, on_single_step as *const () as usize),
            (EVENT_BREAKPOINT, on_breakpoint as *const () as usize),
            (EVENT_METHOD_ENTRY, on_method_entry as *const () as usize),
        ],
    );
    set_mode(jvmti, true, EVENT_BREAKPOINT, 0);
    AGENT = jvmti;
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L7W62ObserverArmedMidLoop_setEvent(
    _env: Env,
    _self: J,
    on: u8,
    event: i32,
    thread: J,
) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    set_mode(AGENT, on != 0, event, thread)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L7W62ObserverArmedMidLoop_breakpoints(env: Env, _self: J, owner: J, on: u8) -> i32 {
    if AGENT.is_null() {
        return -1;
    }
    let method = jni!(env, 113, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        owner,
        c"spinBreak".as_ptr(),
        c"()V".as_ptr(),
    );
    if method == 0 {
        jni!(env, 17, extern "system" fn(Env))(env);
        return -2;
    }
    let mut entries: i32 = 0;
    let mut table: *mut LineEntry = std::ptr::null_mut();
    let err = jvmti!(AGENT, 70, extern "system" fn(Env, J, *mut i32, *mut *mut LineEntry) -> i32)(
        AGENT,
        method,
        &mut entries,
        &mut table,
    );
    if err != 0 {
        return err;
    }
    let slot = if on != 0 { 38 } else { 39 };
    let mut first = 0;
    for i in 0..entries.max(0) as usize {
        let row = &*table.add(i);
        let _ = row.line_number;
        let err = jvmti!(AGENT, slot, extern "system" fn(Env, J, i64) -> i32)(AGENT, method, row.start_location);
        if err != 0 && first == 0 {
            first = err;
        }
    }
    agent::deallocate(AGENT, table as *mut c_char);
    first
}

#[no_mangle]
pub unsafe extern "system" fn Java_L7W62ObserverArmedMidLoop_count(_env: Env, _self: J, which: i32) -> i32 {
    match usize::try_from(which).ok().and_then(|i| COUNTS.get(i)) {
        Some(c) => c.load(Ordering::Relaxed),
        None => -1,
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L7W62ObserverArmedMidLoop_reset(_env: Env, _self: J) {
    for c in COUNTS.iter() {
        c.store(0, Ordering::Relaxed);
    }
}
