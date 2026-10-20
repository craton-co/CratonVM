//! Rust port of tools/probes/interp/L1/L1W56JvmtiThreadTimes.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void, CStr};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";
/// `can_get_current_thread_cpu_time` (bit 22), `can_get_thread_cpu_time`
/// (bit 23).
const CURRENT: u32 = 1 << 22;
const THREAD: u32 = 1 << 23;

static mut AGENT: Env = std::ptr::null_mut();
static mut VM: *mut *const usize = std::ptr::null_mut();
static mut OUT: String = String::new();
static SINK: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

unsafe fn line(text: String) {
    OUT.push_str(&text);
    OUT.push('\n');
}

type Caps = [u32; 4];

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    let mut potential: Caps = [0; 4];
    jvmti!(jvmti, 140, extern "system" fn(Env, *mut Caps) -> i32)(jvmti, &mut potential);
    let wanted: Caps = [CURRENT | THREAD, 0, 0, 0];
    OUT.clear();
    let add = jvmti!(jvmti, 142, extern "system" fn(Env, *const Caps) -> i32)(jvmti, &wanted);
    line(format!(
        "onload: potential current={} thread={} add={add}",
        u32::from(potential[0] & CURRENT != 0),
        u32::from(potential[0] & THREAD != 0)
    ));
    AGENT = jvmti;
    VM = vm;
    0
}

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

#[repr(C)]
struct ThreadInfo {
    name: *mut c_char,
    priority: i32,
    is_daemon: u8,
    thread_group: J,
    context_class_loader: J,
}

#[repr(C)]
struct GroupInfo {
    parent: J,
    name: *mut c_char,
    max_priority: i32,
    is_daemon: u8,
}

type TimerFn = extern "system" fn(Env, *mut TimerInfo) -> i32;
type CurrentTimeFn = extern "system" fn(Env, *mut i64) -> i32;
type ThreadTimeFn = extern "system" fn(Env, J, *mut i64) -> i32;
type ChildrenFn = extern "system" fn(Env, J, *mut i32, *mut *mut J, *mut i32, *mut *mut J) -> i32;

unsafe fn timer_row(label: &str, err: i32, info: &TimerInfo) {
    line(format!(
        "{label}: err={err} max={} forward={} backward={} kind={}",
        info.max_value, info.may_skip_forward, info.may_skip_backward, info.kind
    ));
}

unsafe fn text_of(p: *mut c_char) -> String {
    if p.is_null() {
        "?".into()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W56JvmtiThreadTimes_facts(
    env: Env,
    _self: J,
    other: J,
    unstarted: J,
    dead: J,
    group: J,
    method: J,
) -> J {
    let jvmti = AGENT;
    if jvmti.is_null() {
        return agent::new_string(env, &format!("{NO_ENV}\n"));
    }
    let mut info = TimerInfo::default();
    let err = jvmti!(jvmti, 134, TimerFn)(jvmti, &mut info);
    timer_row("current timer", err, &info);
    let mut info = TimerInfo::default();
    let err = jvmti!(jvmti, 136, TimerFn)(jvmti, &mut info);
    timer_row("thread timer", err, &info);
    let (mut before, mut after): (i64, i64) = (0, 0);
    let err = jvmti!(jvmti, 135, CurrentTimeFn)(jvmti, &mut before);
    for i in 0..200_000_000i64 {
        SINK.fetch_add(i, std::sync::atomic::Ordering::Relaxed);
    }
    let err2 = jvmti!(jvmti, 135, CurrentTimeFn)(jvmti, &mut after);
    line(format!(
        "current time: err={err} err={err2} positive={} advances={}",
        u8::from(before > 0),
        u8::from(after > before)
    ));
    let mut nanos: i64 = -1;
    let err = jvmti!(jvmti, 137, ThreadTimeFn)(jvmti, 0, &mut nanos);
    line(format!("thread time null: err={err} positive={}", u8::from(nanos > 0)));
    nanos = -1;
    let err = jvmti!(jvmti, 137, ThreadTimeFn)(jvmti, other, &mut nanos);
    line(format!("thread time other: err={err} nonneg={}", u8::from(nanos >= 0)));
    let err = jvmti!(jvmti, 137, ThreadTimeFn)(jvmti, unstarted, &mut nanos);
    line(format!("thread time unstarted: err={err}"));
    let err = jvmti!(jvmti, 137, ThreadTimeFn)(jvmti, dead, &mut nanos);
    line(format!("thread time dead: err={err}"));
    let err = jvmti!(jvmti, 137, ThreadTimeFn)(jvmti, group, &mut nanos);
    line(format!("thread time not a thread: err={err}"));
    let a = jvmti!(jvmti, 135, CurrentTimeFn)(jvmti, std::ptr::null_mut());
    let b = jvmti!(jvmti, 137, ThreadTimeFn)(jvmti, 0, std::ptr::null_mut());
    let c = jvmti!(jvmti, 134, TimerFn)(jvmti, std::ptr::null_mut());
    line(format!("null out: current={a} thread={b} timer={c}"));
    if let Some(plain) = agent::get_env(VM, agent::JVMTI_VERSION_1_2) {
        let mut live: Caps = [0; 4];
        jvmti!(plain, 140, extern "system" fn(Env, *mut Caps) -> i32)(plain, &mut live);
        let time = jvmti!(plain, 135, CurrentTimeFn)(plain, &mut nanos);
        let timer = jvmti!(plain, 134, TimerFn)(plain, &mut info);
        line(format!(
            "live env: potential current={} thread={} time={time} timer={timer}",
            u32::from(live[0] & CURRENT != 0),
            u32::from(live[0] & THREAD != 0)
        ));
        jvmti!(plain, 127, extern "system" fn(Env) -> i32)(plain);
    }
    let (mut threads, mut groups): (i32, i32) = (0, 0);
    let (mut thread_list, mut group_list): (*mut J, *mut J) = (std::ptr::null_mut(), std::ptr::null_mut());
    let err = jvmti!(jvmti, 15, ChildrenFn)(jvmti, group, &mut threads, &mut thread_list, &mut groups, &mut group_list);
    line(format!("group children: err={err} threads={threads} groups={groups}"));
    if err == 0 && threads > 0 {
        let mut ti = ThreadInfo { name: std::ptr::null_mut(), priority: 0, is_daemon: 0, thread_group: 0, context_class_loader: 0 };
        jvmti!(jvmti, 9, extern "system" fn(Env, J, *mut ThreadInfo) -> i32)(jvmti, *thread_list, &mut ti);
        line(format!("  thread: {}", text_of(ti.name)));
        agent::deallocate(jvmti, ti.name);
    }
    if err == 0 && groups > 0 {
        let mut gi = GroupInfo { parent: 0, name: std::ptr::null_mut(), max_priority: 0, is_daemon: 0 };
        jvmti!(jvmti, 14, extern "system" fn(Env, J, *mut GroupInfo) -> i32)(jvmti, *group_list, &mut gi);
        line(format!("  group: {}", text_of(gi.name)));
        agent::deallocate(jvmti, gi.name);
    }
    agent::deallocate(jvmti, thread_list.cast());
    agent::deallocate(jvmti, group_list.cast());
    let err = jvmti!(jvmti, 15, ChildrenFn)(jvmti, other, &mut threads, &mut thread_list, &mut groups, &mut group_list);
    line(format!("group children of a thread: err={err}"));
    let mid = jni!(env, 7, extern "system" fn(Env, J) -> J)(env, method);
    let mut obsolete: u8 = 2;
    let err = jvmti!(jvmti, 91, extern "system" fn(Env, J, *mut u8) -> i32)(jvmti, mid, &mut obsolete);
    line(format!("obsolete: err={err} value={obsolete}"));
    let answer = agent::new_string(env, &OUT);
    OUT.clear();
    answer
}
