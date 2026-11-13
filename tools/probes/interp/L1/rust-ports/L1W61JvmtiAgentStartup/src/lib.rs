//! Rust port of tools/probes/interp/L1/L1W61JvmtiAgentStartup.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr, CString};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD_OUT: String = String::new();

/// `jvmtiTimerInfo`.
#[repr(C)]
#[derive(Default)]
struct TimerInfo {
    max_value: i64,
    may_skip_forward: u8,
    may_skip_backward: u8,
    kind: i32,
    reserved1: i64,
    reserved2: i64,
}

type GetProp = extern "system" fn(Env, *const c_char, *mut *mut c_char) -> i32;
type GetProps = extern "system" fn(Env, *mut i32, *mut *mut *mut c_char) -> i32;
type SetProp = extern "system" fn(Env, *const c_char, *const c_char) -> i32;
type GetTimer = extern "system" fn(Env, *mut TimerInfo) -> i32;
type HashCode = extern "system" fn(Env, J, *mut i32) -> i32;
type Size = extern "system" fn(Env, J, *mut i64) -> i32;
type AddSearch = extern "system" fn(Env, *const c_char) -> i32;

fn c(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

unsafe fn keys_of(jvmti: Env) -> Result<Vec<String>, i32> {
    let mut count: i32 = 0;
    let mut keys: *mut *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 130, GetProps)(jvmti, &mut count, &mut keys);
    if err != 0 {
        return Err(err);
    }
    let mut out = Vec::new();
    for i in 0..count as usize {
        out.push(agent::take(jvmti, *keys.add(i)));
    }
    agent::deallocate(jvmti, keys.cast());
    Ok(out)
}

unsafe fn get_prop(jvmti: Env, key: &str) -> Result<String, i32> {
    let key = c(key);
    let mut value: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 131, GetProp)(jvmti, key.as_ptr(), &mut value);
    if err != 0 {
        return Err(err);
    }
    Ok(agent::take(jvmti, value))
}

unsafe fn set_prop(jvmti: Env, key: &str, value: &str) -> i32 {
    let (key, value) = (c(key), c(value));
    jvmti!(jvmti, 132, SetProp)(jvmti, key.as_ptr(), value.as_ptr())
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    let mut out = String::new();
    match get_prop(jvmti, "java.vm.specification.version") {
        Ok(v) => out.push_str(&format!("onload GetSystemProperty java.vm.specification.version: err=0 value={v}\n")),
        Err(e) => out.push_str(&format!("onload GetSystemProperty java.vm.specification.version: err={e} value=-\n")),
    }
    let e = get_prop(jvmti, "java.home").err().unwrap_or(0);
    out.push_str(&format!("onload GetSystemProperty java.home: err={e}\n"));
    let e = get_prop(jvmti, "l1w61.missing").err().unwrap_or(0);
    out.push_str(&format!("onload GetSystemProperty l1w61.missing: err={e}\n"));
    match keys_of(jvmti) {
        Ok(keys) => {
            let has = |k: &str| i32::from(keys.iter().any(|x| x == k));
            out.push_str(&format!(
                "onload GetSystemProperties: err=0 java.home={} java.vm.version={} user.dir={} os.name={}\n",
                has("java.home"),
                has("java.vm.version"),
                has("user.dir"),
                has("os.name")
            ));
        }
        Err(e) => out.push_str(&format!("onload GetSystemProperties: err={e}\n")),
    }
    let e = set_prop(jvmti, "java.vm.version", "x");
    out.push_str(&format!("onload SetSystemProperty java.vm.version: err={e}\n"));
    let e = set_prop(jvmti, "l1w61.missing", "x");
    out.push_str(&format!("onload SetSystemProperty l1w61.missing: err={e}\n"));
    let e = match get_prop(jvmti, "java.library.path") {
        Ok(v) => set_prop(jvmti, "java.library.path", &v),
        Err(e) => e,
    };
    out.push_str(&format!("onload SetSystemProperty java.library.path to itself: err={e}\n"));
    let mut info = TimerInfo::default();
    let e = jvmti!(jvmti, 138, GetTimer)(jvmti, &mut info);
    out.push_str(&format!("onload GetTimerInfo: err={e}\n"));
    ONLOAD_OUT = out;
    0
}

unsafe fn jstr(env: Env, s: J) -> String {
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, s, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, s, chars);
    text
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_onload(env: Env, _self: J) -> J {
    if AGENT.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    agent::new_string(env, &ONLOAD_OUT)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_getProp(env: Env, _self: J, key: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("!{NO_ENV}\n"));
    }
    let key = jstr(env, key);
    match get_prop(jvmti, &key) {
        Ok(v) => agent::new_string(env, &format!("={v}")),
        Err(e) => agent::new_string(env, &format!("!{e}")),
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_propKeys(env: Env, _self: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("!{NO_ENV}\n"));
    }
    match keys_of(jvmti) {
        Ok(keys) => {
            let mut out = String::new();
            for k in keys {
                out.push_str(&k);
                out.push('\n');
            }
            agent::new_string(env, &out)
        }
        Err(e) => agent::new_string(env, &format!("!{e}")),
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_setProp(env: Env, _self: J, key: J, value: J) -> i32 {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return -1;
    }
    let key = jstr(env, key);
    let value = jstr(env, value);
    set_prop(jvmti, &key, &value)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_nullRows(env: Env, _self: J, any: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut out = String::new();
    let mut value: *mut c_char = std::ptr::null_mut();
    let mut keys: *mut *mut c_char = std::ptr::null_mut();
    let mut count: i32 = 0;
    let mut hash: i32 = 0;
    let mut size: i64 = 0;
    let home = c("java.home");
    let x = c("x");
    let rows: [(&str, i32); 12] = [
        ("null GetSystemProperty key", jvmti!(jvmti, 131, GetProp)(jvmti, std::ptr::null(), &mut value)),
        ("null GetSystemProperty value_ptr", jvmti!(jvmti, 131, GetProp)(jvmti, home.as_ptr(), std::ptr::null_mut())),
        ("null GetSystemProperties count_ptr", jvmti!(jvmti, 130, GetProps)(jvmti, std::ptr::null_mut(), &mut keys)),
        ("null GetSystemProperties property_ptr", jvmti!(jvmti, 130, GetProps)(jvmti, &mut count, std::ptr::null_mut())),
        ("null SetSystemProperty key", jvmti!(jvmti, 132, SetProp)(jvmti, std::ptr::null(), x.as_ptr())),
        ("null GetObjectHashCode object", jvmti!(jvmti, 58, HashCode)(jvmti, 0, &mut hash)),
        ("null GetObjectHashCode hash_code_ptr", jvmti!(jvmti, 58, HashCode)(jvmti, any, std::ptr::null_mut())),
        ("null GetObjectSize object", jvmti!(jvmti, 154, Size)(jvmti, 0, &mut size)),
        ("null GetObjectSize size_ptr", jvmti!(jvmti, 154, Size)(jvmti, any, std::ptr::null_mut())),
        ("null GetTimerInfo", jvmti!(jvmti, 138, GetTimer)(jvmti, std::ptr::null_mut())),
        ("null AddToBootstrapClassLoaderSearch", jvmti!(jvmti, 149, AddSearch)(jvmti, std::ptr::null())),
        ("null AddToSystemClassLoaderSearch", jvmti!(jvmti, 151, AddSearch)(jvmti, std::ptr::null())),
    ];
    for (row, err) in rows {
        out.push_str(&format!("{row}: err={err}\n"));
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_hash(_env: Env, _self: J, o: J) -> i64 {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return -1;
    }
    let mut hash: i32 = 0;
    let err = jvmti!(jvmti, 58, HashCode)(jvmti, o, &mut hash);
    (i64::from(err) << 32) | (i64::from(hash) & 0xffff_ffff)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_size(_env: Env, _self: J, o: J) -> i64 {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return -1;
    }
    let mut size: i64 = 0;
    let err = jvmti!(jvmti, 154, Size)(jvmti, o, &mut size);
    if err == 0 {
        size
    } else {
        -i64::from(err)
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_timer(env: Env, _self: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut info = TimerInfo::default();
    let err = jvmti!(jvmti, 138, GetTimer)(jvmti, &mut info);
    agent::new_string(
        env,
        &format!(
            "timer: err={err} max={} forward={} backward={} kind={}\n",
            info.max_value, info.may_skip_forward, info.may_skip_backward, info.kind
        ),
    )
}

unsafe fn add_search(env: Env, path: J, boot: bool) -> i32 {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return -1;
    }
    let path = c(&jstr(env, path));
    jvmti!(jvmti, if boot { 149 } else { 151 }, AddSearch)(jvmti, path.as_ptr())
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_addBoot(env: Env, _self: J, path: J) -> i32 {
    add_search(env, path, true)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W61JvmtiAgentStartup_addSystem(env: Env, _self: J, path: J) -> i32 {
    add_search(env, path, false)
}
