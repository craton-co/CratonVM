//! What every Rust port of a C agent shim needs: the slot-call macros over
//! the JNI and JVMTI function tables, `GetEnv`, `Deallocate`, and the
//! string helpers. Not a crate (each port stays standalone, with no
//! dependencies): a port includes it with
//!
//! ```text
//! #[path = "../../common/agent.rs"]
//! #[macro_use]
//! mod agent;
//! ```
//!
//! Slot numbers: `jvmti!` takes the 1-based function number `jvmti.h`
//! gives each JVMTI function (`SetEventNotificationMode` is 2); `jni!`
//! takes the 0-based index of a function in `JNINativeInterface_`
//! (`FindClass` is 6), as `jni.h` lays them out.
#![allow(dead_code, clippy::missing_safety_doc)]

use std::ffi::{c_char, c_void, CStr, CString};

/// A `jobject` (any reference) or a `jmethodID` / `jfieldID`.
pub type J = usize;
/// A `jvmtiEnv*` or a `JNIEnv*`: a pointer to a pointer to a function table.
pub type Env = *mut *const usize;

/// `JVMTI_VERSION_1_2`, what the C shims ask `GetEnv` for.
pub const JVMTI_VERSION_1_2: i32 = 0x3001_0200;

/// Entry `index` (0-based) of the function table `env` points at.
pub unsafe fn slot(env: Env, index: usize) -> usize {
    *(*env).add(index)
}

/// JVMTI function number `$n` (1-based, as `jvmti.h` numbers them) of
/// `$env`, as a function pointer of type `$ty`.
macro_rules! jvmti {
    ($env:expr, $n:expr, $ty:ty) => {
        std::mem::transmute::<usize, $ty>($crate::agent::slot($env, $n - 1))
    };
}

/// JNI function `$n` (the 0-based index in `JNINativeInterface_`) of
/// `$env`, as a function pointer of type `$ty`.
macro_rules! jni {
    ($env:expr, $n:expr, $ty:ty) => {
        std::mem::transmute::<usize, $ty>($crate::agent::slot($env, $n))
    };
}

/// `(*vm)->GetEnv(vm, &env, version)`: the env, or `None`.
pub unsafe fn get_env(vm: *mut *const usize, version: i32) -> Option<Env> {
    let get_env: extern "system" fn(*mut *const usize, *mut Env, i32) -> i32 =
        std::mem::transmute(*(*vm).add(6));
    let mut env: Env = std::ptr::null_mut();
    if get_env(vm, &mut env, version) != 0 {
        return None;
    }
    Some(env)
}

/// `Deallocate(p)`, nothing for NULL.
pub unsafe fn deallocate(jvmti: Env, p: *mut c_char) {
    if !p.is_null() {
        jvmti!(jvmti, 47, extern "system" fn(Env, *mut c_char) -> i32)(jvmti, p);
    }
}

/// The modified UTF-8 at `p`, which the VM allocated; deallocated after.
pub unsafe fn take(jvmti: Env, p: *mut c_char) -> String {
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    deallocate(jvmti, p);
    s
}

/// `GetClassSignature`'s signature of `klass`; "null" for NULL, "?" on an
/// error.
pub unsafe fn signature_of(jvmti: Env, klass: J) -> String {
    if klass == 0 {
        return "null".into();
    }
    let mut sig: *mut c_char = std::ptr::null_mut();
    if jvmti!(jvmti, 48, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char) -> i32)(
        jvmti,
        klass,
        &mut sig,
        std::ptr::null_mut(),
    ) != 0
    {
        return "?".into();
    }
    take(jvmti, sig)
}

/// `NewStringUTF(text)`.
pub unsafe fn new_string(env: Env, text: &str) -> J {
    let text = CString::new(text).unwrap_or_default();
    jni!(env, 167, extern "system" fn(Env, *const c_char) -> J)(env, text.as_ptr())
}

/// `SetEventCallbacks` with a `jvmtiEventCallbacks` whose member for event
/// `e` (`JVMTI_EVENT_VM_INIT` is 50) is `callbacks[e - 50]`.
pub unsafe fn set_event_callbacks(jvmti: Env, callbacks: &[(i32, usize)]) -> i32 {
    let mut table = [0usize; 39];
    for &(event, f) in callbacks {
        table[(event - 50) as usize] = f;
    }
    jvmti!(jvmti, 122, extern "system" fn(Env, *const usize, i32) -> i32)(
        jvmti,
        table.as_ptr(),
        (table.len() * std::mem::size_of::<usize>()) as i32,
    )
}

/// The opaque `void*` of `SetEnvironmentLocalStorage`.
pub type Storage = *const c_void;
