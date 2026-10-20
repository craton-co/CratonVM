//! Rust port of tools/probes/interp/L1/L1K54JvmtiLambdaProxyLocalsAndFields.c:
//! the same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

/// `can_get_synthetic_attribute` (bit 4) and `can_access_local_variables`
/// (bit 14).
const WANTED: u32 = (1 << 4) | (1 << 14);
const NO_ENV: &str = "no JVMTI env";

static mut AGENT: Env = std::ptr::null_mut();

type Names = extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;
type FieldNames = extern "system" fn(Env, J, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [WANTED, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    AGENT = jvmti;
    0
}

unsafe fn opt(jvmti: Env, p: *mut c_char) -> Option<String> {
    (!p.is_null()).then(|| agent::take(jvmti, p))
}

unsafe fn name_of(jvmti: Env, method: J) -> String {
    let mut name: *mut c_char = std::ptr::null_mut();
    let mut sig: *mut c_char = std::ptr::null_mut();
    if jvmti!(jvmti, 64, Names)(jvmti, method, &mut name, &mut sig, std::ptr::null_mut()) != 0 {
        return "?".into();
    }
    let name = opt(jvmti, name).unwrap_or_default();
    let sig = opt(jvmti, sig).unwrap_or_default();
    format!("{name}{sig}")
}

unsafe fn object_class(env: Env, o: J) -> J {
    jni!(env, 31, extern "system" fn(Env, J) -> J)(env, o)
}

unsafe fn same(env: Env, a: J, b: J) -> i32 {
    if a == 0 || b == 0 {
        return 0;
    }
    i32::from(jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, a, b) != 0)
}

unsafe fn current_thread(jvmti: Env) -> J {
    let mut thread: J = 0;
    jvmti!(jvmti, 18, extern "system" fn(Env, *mut J) -> i32)(jvmti, &mut thread);
    thread
}

unsafe fn frame_location(jvmti: Env, thread: J, depth: i32) -> (i32, J, i64) {
    let mut method: J = 0;
    let mut location: i64 = -1;
    let err = jvmti!(jvmti, 19, extern "system" fn(Env, J, i32, *mut J, *mut i64) -> i32)(
        jvmti,
        thread,
        depth,
        &mut method,
        &mut location,
    );
    (err, method, location)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLambdaProxyLocalsAndFields_fields(env: Env, _cls: J, lambda: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let klass = object_class(env, lambda);
    let mut count: i32 = 0;
    let mut fields: *mut J = std::ptr::null_mut();
    let err = jvmti!(jvmti, 53, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        klass,
        &mut count,
        &mut fields,
    );
    let mut out = format!("err={err} count={}", if err == 0 { count } else { 0 });
    for i in 0..if err == 0 { count.max(0) as usize } else { 0 } {
        let field = *fields.add(i);
        let (mut name, mut sig, mut generic) = (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        let e_name = jvmti!(jvmti, 60, FieldNames)(jvmti, klass, field, &mut name, &mut sig, &mut generic);
        let mut mods: i32 = 0;
        let e_mods = jvmti!(jvmti, 62, extern "system" fn(Env, J, J, *mut i32) -> i32)(jvmti, klass, field, &mut mods);
        let mut declaring: J = 0;
        let e_decl =
            jvmti!(jvmti, 61, extern "system" fn(Env, J, J, *mut J) -> i32)(jvmti, klass, field, &mut declaring);
        let mut synthetic: u8 = 0;
        let e_syn =
            jvmti!(jvmti, 63, extern "system" fn(Env, J, J, *mut u8) -> i32)(jvmti, klass, field, &mut synthetic);
        let name = opt(jvmti, name);
        let sig = opt(jvmti, sig);
        let generic = opt(jvmti, generic);
        let value = match sig.as_deref() {
            _ if e_name != 0 => "?".to_string(),
            None => "?".to_string(),
            Some(s) if s.starts_with('J') => {
                jni!(env, 101, extern "system" fn(Env, J, J) -> i64)(env, lambda, field).to_string()
            }
            Some(s) if s.starts_with('I') => {
                jni!(env, 100, extern "system" fn(Env, J, J) -> i32)(env, lambda, field).to_string()
            }
            Some("Ljava/lang/String;") => {
                let s = jni!(env, 95, extern "system" fn(Env, J, J) -> J)(env, lambda, field);
                if s == 0 {
                    "\"null\"".to_string()
                } else {
                    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(
                        env,
                        s,
                        std::ptr::null_mut(),
                    );
                    if chars.is_null() {
                        "\"null\"".to_string()
                    } else {
                        let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
                        jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, s, chars);
                        format!("\"{text}\"")
                    }
                }
            }
            Some(_) => {
                let o = jni!(env, 95, extern "system" fn(Env, J, J) -> J)(env, lambda, field);
                if o == 0 {
                    "null".to_string()
                } else {
                    match agent::signature_of(jvmti, object_class(env, o)).as_str() {
                        "?" => "?".to_string(),
                        s => format!("a {s}"),
                    }
                }
            }
        };
        out.push_str(&format!(
            "\n[{i}] name err={e_name} {} {} generic={} modifiers err={e_mods} 0x{mods:x} declaring err={e_decl} own={} synthetic err={e_syn} {synthetic} value={value}",
            name.as_deref().unwrap_or("?"),
            sig.as_deref().unwrap_or("?"),
            generic.as_deref().unwrap_or("null"),
            same(env, declaring, klass),
        ));
    }
    if !fields.is_null() {
        agent::deallocate(jvmti, fields as *mut c_char);
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLambdaProxyLocalsAndFields_locals(
    env: Env,
    _cls: J,
    lambda: J,
    arg: J,
    depth: i32,
) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let thread = current_thread(jvmti);
    let (err, method, location) = frame_location(jvmti, thread, depth);
    let name = name_of(jvmti, method);
    let mut out = format!("depth {depth}: err={err} {} at {location}", if err == 0 { name.as_str() } else { "-" });
    let get_object = jvmti!(jvmti, 21, extern "system" fn(Env, J, i32, i32, *mut J) -> i32);
    let get_int = jvmti!(jvmti, 22, extern "system" fn(Env, J, i32, i32, *mut i32) -> i32);
    let mut obj: J = 0;
    let err = jvmti!(jvmti, 155, extern "system" fn(Env, J, i32, *mut J) -> i32)(jvmti, thread, depth, &mut obj);
    out.push_str(&format!("\ninstance: err={err} same={}", same(env, obj, lambda)));
    obj = 0;
    let err = get_object(jvmti, thread, depth, 0, &mut obj);
    out.push_str(&format!("\nslot 0 object: err={err} same={}", same(env, obj, lambda)));
    obj = 0;
    let err = get_object(jvmti, thread, depth, 1, &mut obj);
    out.push_str(&format!("\nslot 1 object: err={err} arg={}", same(env, obj, arg)));
    let mut value: i32 = 0;
    let err = get_int(jvmti, thread, depth, 1, &mut value);
    out.push_str(&format!("\nslot 1 int: err={err} value={value}"));
    value = 0;
    let err = get_int(jvmti, thread, depth, 2, &mut value);
    out.push_str(&format!("\nslot 2 int: err={err} value={value}"));
    obj = 0;
    let err = get_object(jvmti, thread, depth, 9, &mut obj);
    out.push_str(&format!("\nslot 9 object: err={err}"));
    let mut entries: i32 = 0;
    let mut table: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(jvmti, 72, extern "system" fn(Env, J, *mut i32, *mut *mut c_char) -> i32)(
        jvmti,
        method,
        &mut entries,
        &mut table,
    );
    out.push_str(&format!("\nvariable table: err={err} count={}", if err == 0 { entries } else { 0 }));
    agent::deallocate(jvmti, table);
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLambdaProxyLocalsAndFields_frames(env: Env, _cls: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, NO_ENV);
    }
    let thread = current_thread(jvmti);
    let mut count: i32 = 0;
    let e_count = jvmti!(jvmti, 16, extern "system" fn(Env, J, *mut i32) -> i32)(jvmti, thread, &mut count);
    let (e_two, two, location) = frame_location(jvmti, thread, 2);
    let two = name_of(jvmti, two);
    let (e_three, three, _) = frame_location(jvmti, thread, 3);
    let three = name_of(jvmti, three);
    agent::new_string(
        env,
        &format!("count err={e_count} {count}; depth 2 err={e_two} {two} at {location}; depth 3 err={e_three} {three}"),
    )
}
