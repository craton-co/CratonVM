//! Rust port of tools/probes/interp/L1/L1W52JvmtiExceptionCatch.c.
#![allow(non_snake_case, static_mut_refs)]
use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt::Write as _;

type J = usize;
type Env = *mut *const usize;

static mut AGENT: Env = std::ptr::null_mut();
static mut ONLOAD: (i32, i32) = (-1, -1);
static mut PROBE: J = 0;
static mut HOT: i32 = 0;
static mut OUT: String = String::new();

unsafe fn slot(env: Env, index: usize) -> usize {
    *(*env).add(index)
}
macro_rules! f {
    ($env:expr, $idx:expr, $ty:ty) => {
        std::mem::transmute::<usize, $ty>(slot($env, $idx))
    };
}

unsafe extern "system" fn on_catch(jvmti: Env, env: Env, _t: J, method: usize, location: i64, exception: J) {
    if PROBE == 0 {
        return;
    }
    let mut declaring: J = 0;
    if f!(jvmti, 64, extern "system" fn(Env, usize, *mut J) -> i32)(jvmti, method, &mut declaring) != 0
        || f!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, declaring, PROBE) == 0
    {
        return;
    }
    if exception == 0 {
        let _ = writeln!(OUT, "catch: no exception object");
        return;
    }
    let class = f!(env, 31, extern "system" fn(Env, J) -> J)(env, exception);
    let gm = f!(env, 33, extern "system" fn(Env, J, *const c_char, *const c_char) -> usize)(
        env, class, b"getMessage\0".as_ptr().cast(), b"()Ljava/lang/String;\0".as_ptr().cast());
    let message = f!(env, 36, extern "system" fn(Env, J, usize, *const u64) -> J)(env, exception, gm, std::ptr::null());
    let mut text = "null".to_string();
    let mut chars: *const c_char = std::ptr::null();
    if message != 0 {
        chars = f!(env, 169, extern "system" fn(Env, J, *mut u8) -> *const c_char)(env, message, std::ptr::null_mut());
        text = CStr::from_ptr(chars).to_string_lossy().into_owned();
    }
    if text == "hot" {
        HOT += 1;
    } else {
        let mut name: *mut c_char = std::ptr::null_mut();
        let mut sig: *mut c_char = std::ptr::null_mut();
        f!(jvmti, 63, extern "system" fn(Env, usize, *mut *mut c_char, *mut c_void, *mut c_void) -> i32)(
            jvmti, method, &mut name, std::ptr::null_mut(), std::ptr::null_mut());
        f!(jvmti, 47, extern "system" fn(Env, J, *mut *mut c_char, *mut c_void) -> i32)(
            jvmti, class, &mut sig, std::ptr::null_mut());
        let _ = writeln!(
            OUT,
            "catch in {} at {location}: {} \"{text}\"",
            CStr::from_ptr(name).to_string_lossy(),
            CStr::from_ptr(sig).to_string_lossy()
        );
        f!(jvmti, 46, extern "system" fn(Env, *mut c_char) -> i32)(jvmti, name);
        f!(jvmti, 46, extern "system" fn(Env, *mut c_char) -> i32)(jvmti, sig);
    }
    if message != 0 {
        f!(env, 170, extern "system" fn(Env, J, *const c_char))(env, message, chars);
    }
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _o: *mut c_char, _r: *mut c_void) -> i32 {
    let get_env: extern "system" fn(*mut *const usize, *mut Env, i32) -> i32 =
        std::mem::transmute(*(*vm).add(6));
    let mut env: Env = std::ptr::null_mut();
    if get_env(vm, &mut env, 0x3001_0200) != 0 {
        return 1;
    }
    AGENT = env;
    let caps: [u32; 4] = [1 << 17, 0, 0, 0];
    let a = f!(env, 141, extern "system" fn(Env, *const [u32; 4]) -> i32)(env, &caps);
    let mut callbacks = [0usize; 39];
    callbacks[9] = on_catch as usize;
    let b = f!(env, 121, extern "system" fn(Env, *const usize, i32) -> i32)(env, callbacks.as_ptr(), (39 * 8) as i32);
    ONLOAD = (a, b);
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W52JvmtiExceptionCatch_rows(env: Env, this: J) -> J {
    OUT.clear();
    let (a, b) = ONLOAD;
    let _ = writeln!(OUT, "onload: caps err={a} callbacks err={b}");
    PROBE = f!(env, 21, extern "system" fn(Env, J) -> J)(env, this);
    let mut thread: J = 0;
    f!(AGENT, 17, extern "system" fn(Env, *mut J) -> i32)(AGENT, &mut thread);
    let mode = f!(AGENT, 1, extern "system" fn(Env, i32, i32, J) -> i32);
    let e = mode(AGENT, 1, 59, thread);
    let _ = writeln!(OUT, "enable: err={e}");
    let ex = f!(env, 113, extern "system" fn(Env, J, *const c_char, *const c_char) -> usize)(
        env, this, b"exercise\0".as_ptr().cast(), b"()V\0".as_ptr().cast());
    f!(env, 143, extern "system" fn(Env, J, usize, *const u64))(env, this, ex, std::ptr::null());
    let d = mode(AGENT, 0, 59, thread);
    let _ = writeln!(OUT, "disable: err={d}");
    let _ = writeln!(OUT, "hot catches: {}", HOT);
    let text = CString::new(OUT.clone()).unwrap();
    f!(env, 167, extern "system" fn(Env, *const c_char) -> J)(env, text.as_ptr())
}
