//! Rust port of tools/probes/interp/L1/L1K54JvmtiLambdaProxyConstantPool.c:
//! the same calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

static mut AGENT: Env = std::ptr::null_mut();

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _o: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    // can_get_bytecodes (bit 3); can_get_constant_pool (bit 35 = word 1 bit 3)
    let wanted: [u32; 4] = [1 << 3, 1 << 3, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &wanted);
    AGENT = jvmti;
    0
}

fn mask(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("0x") {
        out.push_str(&rest[..at]);
        out.push_str("0x?");
        rest = rest[at + 2..].trim_start_matches(|c: char| c.is_ascii_hexdigit());
    }
    out.push_str(rest);
    out
}

fn decode(bytes: &[u8], count: i32) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = 0usize;
    let mut i = 1;
    let u2 = |p: usize| u16::from_be_bytes([bytes[p], bytes[p + 1]]);
    while i < count && p < bytes.len() {
        let tag = bytes[p];
        let (text, len, wide) = match tag {
            1 => {
                let n = u2(p + 1) as usize;
                (format!("Utf8 {}", mask(&String::from_utf8_lossy(&bytes[p + 3..p + 3 + n]))), 3 + n, false)
            }
            3 => (format!("Integer {}", i32::from_be_bytes([bytes[p + 1], bytes[p + 2], bytes[p + 3], bytes[p + 4]])), 5, false),
            4 => ("Float".to_string(), 5, false),
            5 => ("Long".to_string(), 9, true),
            6 => ("Double".to_string(), 9, true),
            7 => (format!("Class #{}", u2(p + 1)), 3, false),
            8 => (format!("String #{}", u2(p + 1)), 3, false),
            9 => (format!("Fieldref #{} #{}", u2(p + 1), u2(p + 3)), 5, false),
            10 => (format!("Methodref #{} #{}", u2(p + 1), u2(p + 3)), 5, false),
            11 => (format!("InterfaceMethodref #{} #{}", u2(p + 1), u2(p + 3)), 5, false),
            12 => (format!("NameAndType #{} #{}", u2(p + 1), u2(p + 3)), 5, false),
            15 => (format!("MethodHandle {} #{}", bytes[p + 1], u2(p + 2)), 4, false),
            16 => (format!("MethodType #{}", u2(p + 1)), 3, false),
            17 => (format!("Dynamic {} #{}", u2(p + 1), u2(p + 3)), 5, false),
            18 => (format!("InvokeDynamic {} #{}", u2(p + 1), u2(p + 3)), 5, false),
            _ => (format!("?tag {tag}"), 1, false),
        };
        out.push(format!("#{i} {text}"));
        p += len;
        i += if wide { 2 } else { 1 };
    }
    out
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1K54JvmtiLambdaProxyConstantPool_dump(env: Env, _cls: J, lambda: J) -> J {
    let jvmti = AGENT;
    let klass = jni!(env, 31, extern "system" fn(Env, J) -> J)(env, lambda);
    let mut count: i32 = 0;
    let mut nbytes: i32 = 0;
    let mut bytes: *mut u8 = std::ptr::null_mut();
    let err = jvmti!(jvmti, 146, extern "system" fn(Env, J, *mut i32, *mut i32, *mut *mut u8) -> i32)(
        jvmti, klass, &mut count, &mut nbytes, &mut bytes,
    );
    let mut out = format!("cp err={err} count={count} bytes={nbytes}");
    if err == 0 {
        let slice = std::slice::from_raw_parts(bytes, nbytes as usize);
        for line in decode(slice, count) {
            out.push('\n');
            out.push_str(&line);
        }
    }
    let mut mcount: i32 = 0;
    let mut methods: *mut J = std::ptr::null_mut();
    jvmti!(jvmti, 52, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(jvmti, klass, &mut mcount, &mut methods);
    for k in 0..mcount as usize {
        let m = *methods.add(k);
        let mut name: *mut c_char = std::ptr::null_mut();
        let mut sig: *mut c_char = std::ptr::null_mut();
        jvmti!(jvmti, 64, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
            jvmti, m, &mut name, &mut sig, std::ptr::null_mut(),
        );
        let name = agent::take(jvmti, name);
        let sig = agent::take(jvmti, sig);
        let mut size: i32 = 0;
        let mut code: *mut u8 = std::ptr::null_mut();
        let berr = jvmti!(jvmti, 75, extern "system" fn(Env, J, *mut i32, *mut *mut u8) -> i32)(jvmti, m, &mut size, &mut code);
        let hex = if berr == 0 {
            (0..size as usize).map(|i| format!("{:02x}", *code.add(i))).collect::<Vec<_>>().join("")
        } else {
            format!("err={berr}")
        };
        out.push_str(&format!("\nmethod {name}{sig} code={hex}"));
    }
    agent::new_string(env, &out)
}
