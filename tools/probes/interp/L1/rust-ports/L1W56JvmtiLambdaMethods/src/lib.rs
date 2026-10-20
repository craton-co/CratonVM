//! Rust port of tools/probes/interp/L1/L1W56JvmtiLambdaMethods.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_get_bytecodes` (bit 3), `can_get_synthetic_attribute` (bit 4),
/// `can_get_line_numbers` (bit 12).
const WANTED: u32 = (1 << 3) | (1 << 4) | (1 << 12);

static mut AGENT: Env = std::ptr::null_mut();
static mut OUT: String = String::new();

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

/// `text` with every hex digit after "0x" as 'h'.
fn mask(text: &str) -> String {
    let Some(at) = text.find("0x") else {
        return text.to_string();
    };
    let (head, tail) = text.split_at(at + 2);
    let digits = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
    format!("{head}{}{}", "h".repeat(digits), &tail[digits..])
}

unsafe fn text_of(p: *mut c_char) -> String {
    if p.is_null() {
        "-".into()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

unsafe fn same(env: Env, a: J, b: J) -> bool {
    jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, a, b) != 0
}

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let wanted: [u32; 4] = [WANTED, 0, 0, 0];
    OUT.clear();
    let add = jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    line(format!("onload: add={add}"));
    AGENT = jvmti;
    0
}

type Names = extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32;
type IntOf = extern "system" fn(Env, J, *mut i32) -> i32;
type BoolOf = extern "system" fn(Env, J, *mut u8) -> i32;

unsafe fn method_row(env: Env, jvmti: Env, klass: J, i: i32, m: J) {
    let mut name: *mut c_char = std::ptr::null_mut();
    let mut sig: *mut c_char = std::ptr::null_mut();
    let mut generic: *mut c_char = std::ptr::null_mut();
    let mut declaring: J = 0;
    let mut modifiers: i32 = 0;
    let mut native: u8 = 0;
    let mut synthetic: u8 = 0;
    let mut locals: i32 = 0;
    let mut args: i32 = 0;
    let (mut start, mut end): (i64, i64) = (0, 0);
    let mut lines: i32 = 0;
    let mut table: *mut c_char = std::ptr::null_mut();
    let mut size: i32 = 0;
    let mut bytes: *mut u8 = std::ptr::null_mut();
    let nerr = jvmti!(jvmti, 64, Names)(jvmti, m, &mut name, &mut sig, &mut generic);
    let derr = jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, m, &mut declaring);
    let merr = jvmti!(jvmti, 66, IntOf)(jvmti, m, &mut modifiers);
    let naerr = jvmti!(jvmti, 76, BoolOf)(jvmti, m, &mut native);
    let serr = jvmti!(jvmti, 77, BoolOf)(jvmti, m, &mut synthetic);
    let lerr = jvmti!(jvmti, 68, IntOf)(jvmti, m, &mut locals);
    let aerr = jvmti!(jvmti, 69, IntOf)(jvmti, m, &mut args);
    let locerr = jvmti!(jvmti, 71, extern "system" fn(Env, J, *mut i64, *mut i64) -> i32)(
        jvmti, m, &mut start, &mut end,
    );
    let lnerr = jvmti!(jvmti, 70, extern "system" fn(Env, J, *mut i32, *mut *mut c_char) -> i32)(
        jvmti, m, &mut lines, &mut table,
    );
    let berr = jvmti!(jvmti, 75, extern "system" fn(Env, J, *mut i32, *mut *mut u8) -> i32)(
        jvmti, m, &mut size, &mut bytes,
    );
    let (shown_name, shown_sig) = if nerr == 0 { (text_of(name), text_of(sig)) } else { ("-".into(), String::new()) };
    line(format!(
        "  [{i}] name err={nerr} {shown_name}{shown_sig} generic={}",
        if generic.is_null() { "null" } else { "set" }
    ));
    let same_class = if derr == 0 { i32::from(same(env, declaring, klass)) } else { -1 };
    line(format!(
        "  [{i}] declaring err={derr} same={same_class} modifiers err={merr} 0x{:x} native err={naerr} {native} synthetic err={serr} {synthetic}",
        modifiers as u32
    ));
    let first = if berr == 0 && size > 0 { i32::from(*bytes) } else { -1 };
    line(format!(
        "  [{i}] locals err={lerr} {locals} args err={aerr} {args} location err={locerr} {start}..{end} lines err={lnerr} {lines} bytecodes err={berr} {size} first={first}"
    ));
    agent::deallocate(jvmti, name);
    agent::deallocate(jvmti, sig);
    agent::deallocate(jvmti, generic);
    agent::deallocate(jvmti, table);
    agent::deallocate(jvmti, bytes.cast());
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W56JvmtiLambdaMethods_methods(env: Env, _self: J, klass: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut count: i32 = 0;
    let mut methods: *mut J = std::ptr::null_mut();
    let err = jvmti!(jvmti, 52, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(
        jvmti,
        klass,
        &mut count,
        &mut methods,
    );
    line(format!("methods: err={err} count={count}"));
    if err == 0 {
        for i in 0..count.max(0) {
            method_row(env, jvmti, klass, i, *methods.add(i as usize));
        }
    }
    agent::deallocate(jvmti, methods.cast());
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrameInfo {
    method: J,
    location: i64,
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W56JvmtiLambdaMethods_stack(env: Env, _self: J) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut frames = [FrameInfo { method: 0, location: 0 }; 4];
    let mut count: i32 = 0;
    let err = jvmti!(jvmti, 104, extern "system" fn(Env, J, i32, i32, *mut FrameInfo, *mut i32) -> i32)(
        jvmti,
        0,
        0,
        4,
        frames.as_mut_ptr(),
        &mut count,
    );
    line(format!("stack: err={err} count={count}"));
    if err == 0 {
        for i in 1..count.max(0) {
            let m = frames[i as usize].method;
            let mut declaring: J = 0;
            jvmti!(jvmti, 65, extern "system" fn(Env, J, *mut J) -> i32)(jvmti, m, &mut declaring);
            let csig = agent::signature_of(jvmti, declaring);
            let mut name: *mut c_char = std::ptr::null_mut();
            jvmti!(jvmti, 64, Names)(jvmti, m, &mut name, std::ptr::null_mut(), std::ptr::null_mut());
            let shown = if name.is_null() { "?".to_string() } else { agent::take(jvmti, name) };
            line(format!("  frame {i}: {} {shown}", mask(&csig)));
        }
    }
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
