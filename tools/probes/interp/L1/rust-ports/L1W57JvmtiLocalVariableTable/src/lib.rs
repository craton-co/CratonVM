//! Rust port of tools/probes/interp/L1/L1W57JvmtiLocalVariableTable.c: the
//! same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_access_local_variables`, the 15th bit field.
const CAN_ACCESS_LOCAL_VARIABLES: u32 = 1 << 14;

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD_ADD: i32 = -1;

/// `jvmtiLocalVariableEntry`.
#[repr(C)]
struct Entry {
    start_location: i64,
    length: i32,
    name: *mut c_char,
    signature: *mut c_char,
    generic_signature: *mut c_char,
    slot: i32,
}

type GetLvt = extern "system" fn(Env, J, *mut i32, *mut *mut Entry) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0];
    ONLOAD_ADD = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    AGENT = jvmti;
    0
}

unsafe fn text_or_null(p: *mut c_char) -> String {
    if p.is_null() {
        "null".into()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn from_reflected(env: Env, m: J) -> J {
    jni!(env, 7, extern "system" fn(Env, J) -> J)(env, m)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W57JvmtiLocalVariableTable_table(env: Env, _self: J, label: J, m: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let method = from_reflected(env, m);
    let mut count: i32 = -1;
    let mut table: *mut Entry = std::ptr::null_mut();
    let err = jvmti!(jvmti, 72, GetLvt)(jvmti, method, &mut count, &mut table);
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, label, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, label, chars);
    let shown = if err == 0 { count } else { 0 };
    let mut out = format!("{text}: onload={ONLOAD_ADD} err={err} count={shown}\n");
    if err == 0 {
        for i in 0..count {
            let e = &*table.add(usize::try_from(i).unwrap_or(0));
            out.push_str(&format!(
                "  [{i}] start={} length={} slot={} {} {} generic={}\n",
                e.start_location,
                e.length,
                e.slot,
                text_or_null(e.name),
                text_or_null(e.signature),
                text_or_null(e.generic_signature)
            ));
            agent::deallocate(jvmti, e.name);
            agent::deallocate(jvmti, e.signature);
            agent::deallocate(jvmti, e.generic_signature);
        }
        agent::deallocate(jvmti, table.cast::<c_char>());
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W57JvmtiLocalVariableTable_errors(env: Env, _self: J, m: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let method = from_reflected(env, m);
    let mut count: i32 = -1;
    let mut table: *mut Entry = std::ptr::null_mut();
    let lvt = jvmti!(jvmti, 72, GetLvt);
    let null_method = lvt(jvmti, 0, &mut count, &mut table);
    let null_count = lvt(jvmti, method, std::ptr::null_mut(), &mut table);
    let null_table = lvt(jvmti, method, &mut count, std::ptr::null_mut());
    let (mut live_get, mut live_err) = (-1, -1);
    let mut vm: *mut *const usize = std::ptr::null_mut();
    if jni!(env, 219, extern "system" fn(Env, *mut *mut *const usize) -> i32)(env, &mut vm) == 0 {
        let get_env: extern "system" fn(*mut *const usize, *mut Env, i32) -> i32 = std::mem::transmute(*(*vm).add(6));
        let mut live: Env = std::ptr::null_mut();
        live_get = get_env(vm, &mut live, agent::JVMTI_VERSION_1_2);
        if live_get == 0 {
            live_err = jvmti!(live, 72, GetLvt)(live, method, &mut count, &mut table);
            jvmti!(live, 127, extern "system" fn(Env) -> i32)(live);
        }
    }
    agent::new_string(
        env,
        &format!(
            "errors: null-method={null_method} null-count={null_count} null-table={null_table} live-env={live_get}/{live_err}\n"
        ),
    )
}
