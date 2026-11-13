//! Rust port of tools/probes/interp/L1/L1W57JvmtiRedefine.c: the same calls
//! in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr, CString};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_redefine_classes`, the 10th bit field.
const CAN_REDEFINE_CLASSES: u32 = 1 << 9;

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD_ADD: i32 = -1;

/// `jvmtiClassDefinition`.
#[repr(C)]
struct ClassDef {
    klass: J,
    class_byte_count: i32,
    class_bytes: *const u8,
}

type Redefine = extern "system" fn(Env, i32, *const ClassDef) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [CAN_REDEFINE_CLASSES, 0, 0, 0];
    ONLOAD_ADD = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    AGENT = jvmti;
    0
}

unsafe fn redefine_one(jvmti: Env, env: Env, k: J, bytes: J) -> i32 {
    let count = if bytes == 0 {
        3
    } else {
        jni!(env, 171, extern "system" fn(Env, J) -> i32)(env, bytes)
    };
    let elems: *mut u8 = if bytes == 0 {
        std::ptr::null_mut()
    } else {
        jni!(env, 184, extern "system" fn(Env, J, *mut u8) -> *mut u8)(env, bytes, std::ptr::null_mut())
    };
    let def = ClassDef {
        klass: k,
        class_byte_count: count,
        class_bytes: elems,
    };
    let err = jvmti!(jvmti, 87, Redefine)(jvmti, 1, &def);
    if !elems.is_null() {
        // JNI_ABORT
        jni!(env, 192, extern "system" fn(Env, J, *mut u8, i32))(env, bytes, elems, 2);
    }
    err
}

unsafe fn find_class(env: Env, name: &str) -> J {
    let name = CString::new(name).unwrap_or_default();
    jni!(env, 6, extern "system" fn(Env, *const c_char) -> J)(env, name.as_ptr())
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W57JvmtiRedefine_redefine(env: Env, _self: J, label: J, k: J, bytes: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let err = redefine_one(jvmti, env, k, bytes);
    let chars = jni!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, label, std::ptr::null_mut());
    let text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    jni!(env, 170, extern "system" fn(Env, J, *const c_char))(env, label, chars);
    agent::new_string(env, &format!("{text}: onload={ONLOAD_ADD} err={err}\n"))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W57JvmtiRedefine_errors(env: Env, _self: J, k: J, bytes: J, other: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let redefine = jvmti!(jvmti, 87, Redefine);
    let def = ClassDef {
        klass: k,
        class_byte_count: 0,
        class_bytes: std::ptr::null(),
    };
    let negative = redefine(jvmti, -1, &def);
    let null_defs = redefine(jvmti, 1, std::ptr::null());
    let null_class = redefine_one(jvmti, env, 0, bytes);
    let null_bytes = redefine_one(jvmti, env, k, 0);
    let integer = find_class(env, "java/lang/Integer");
    let type_name = CString::new("TYPE").unwrap_or_default();
    let type_sig = CString::new("Ljava/lang/Class;").unwrap_or_default();
    let field = jni!(env, 144, extern "system" fn(Env, J, *const c_char, *const c_char) -> J)(
        env,
        integer,
        type_name.as_ptr(),
        type_sig.as_ptr(),
    );
    let int_class = jni!(env, 145, extern "system" fn(Env, J, J) -> J)(env, integer, field);
    let primitive = redefine_one(jvmti, env, int_class, bytes);
    let array_class = find_class(env, "[Ljava/lang/String;");
    let array = redefine_one(jvmti, env, array_class, bytes);
    let garbage = jni!(env, 176, extern "system" fn(Env, i32) -> J)(env, 3);
    let not_a_class = redefine_one(jvmti, env, k, garbage);
    let other_class = redefine_one(jvmti, env, k, other);
    let (mut live_get, mut live_potential, mut live_err) = (-1, -1, -1);
    let mut vm: *mut *const usize = std::ptr::null_mut();
    if jni!(env, 219, extern "system" fn(Env, *mut *mut *const usize) -> i32)(env, &mut vm) == 0 {
        let get_env: extern "system" fn(*mut *const usize, *mut Env, i32) -> i32 = std::mem::transmute(*(*vm).add(6));
        let mut live: Env = std::ptr::null_mut();
        live_get = get_env(vm, &mut live, agent::JVMTI_VERSION_1_2);
        if live_get == 0 {
            let mut caps: [u32; 4] = [0; 4];
            jvmti!(live, 140, extern "system" fn(Env, *mut [u32; 4]) -> i32)(live, &mut caps);
            live_potential = i32::from(caps[0] & CAN_REDEFINE_CLASSES != 0);
            live_err = redefine_one(live, env, k, bytes);
            jvmti!(live, 127, extern "system" fn(Env) -> i32)(live);
        }
    }
    agent::new_string(
        env,
        &format!(
            "errors: negative={negative} null-defs={null_defs} null-class={null_class} null-bytes={null_bytes} primitive={primitive} array={array} not-a-class={not_a_class} other-class={other_class} live-env={live_get} potential={live_potential} err={live_err}\n"
        ),
    )
}
