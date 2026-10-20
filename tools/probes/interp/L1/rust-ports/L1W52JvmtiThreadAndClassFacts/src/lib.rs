//! Rust port of tools/probes/interp/L1/L1W52JvmtiThreadAndClassFacts.c, to
//! measure HotSpot on a box without a C compiler. Same calls, same order,
//! same output format.
#![allow(clippy::missing_safety_doc, non_snake_case, static_mut_refs)]
use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt::Write as _;

type J = usize; // jobject
type Env = *mut *const usize; // jvmtiEnv* / JNIEnv*

static mut AGENT: Env = std::ptr::null_mut();
static mut MONITOR: usize = 0;
static mut COOKIE: i32 = 52;
static mut ONLOAD: (i32, i32, i32) = (-1, -1, -1);
static mut ONLOAD_EVENTS: i32 = -1;
static mut THREAD_EVENTS: String = String::new();

unsafe fn record(what: &str, thread: J) {
    let mut info: ThreadInfo = std::mem::zeroed();
    if thread_info(thread, &mut info) != 0 {
        return;
    }
    let name = take(info.name);
    if name.starts_with("facts-") {
        THREAD_EVENTS.push_str(&format!(" {what} {name}"));
    }
}
unsafe extern "system" fn on_start(_j: Env, _e: Env, t: J) {
    record("start", t);
}
unsafe extern "system" fn on_end(_j: Env, _e: Env, t: J) {
    record("end", t);
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W52JvmtiThreadAndClassFacts_threadEvents(env: Env, _s: J) -> J {
    let text = CString::new(format!("thread events:{}\n", THREAD_EVENTS)).unwrap();
    std::mem::transmute::<usize, extern "system" fn(Env, *const c_char) -> J>(slot(env, 167))(env, text.as_ptr())
}

unsafe fn slot(env: Env, index: usize) -> usize {
    *(*env).add(index)
}
macro_rules! jvmti {
    ($n:expr, $ty:ty) => {
        std::mem::transmute::<usize, $ty>(slot(AGENT, $n - 1))
    };
}
macro_rules! jni {
    ($env:expr, $n:expr, $ty:ty) => {
        std::mem::transmute::<usize, $ty>(slot($env, $n))
    };
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
    let caps: [u32; 4] = [(1 << 11) | (1 << 4) | (1 << 3), 0, 0, 0];
    let a = jvmti!(142, extern "system" fn(Env, *const [u32; 4]) -> i32)(env, &caps);
    let b = jvmti!(31, extern "system" fn(Env, *const c_char, *mut usize) -> i32)(
        env,
        b"l1w52-facts\0".as_ptr().cast(),
        std::ptr::addr_of_mut!(MONITOR),
    );
    let c = jvmti!(148, extern "system" fn(Env, *const c_void) -> i32)(
        env,
        std::ptr::addr_of!(COOKIE).cast(),
    );
    ONLOAD = (a, b, c);
    let mut callbacks = [0usize; 39];
    callbacks[2] = on_start as usize;
    callbacks[3] = on_end as usize;
    let e1 = jvmti!(122, extern "system" fn(Env, *const usize, i32) -> i32)(env, callbacks.as_ptr(), 39 * 8);
    let mode = jvmti!(2, extern "system" fn(Env, i32, i32, J) -> i32);
    let e2 = mode(env, 1, 52, 0);
    let e3 = mode(env, 1, 53, 0);
    ONLOAD_EVENTS = if e1 != 0 { e1 } else if e2 != 0 { e2 } else { e3 };
    0
}

unsafe fn dealloc(p: *mut c_char) {
    if !p.is_null() {
        jvmti!(47, extern "system" fn(Env, *mut c_char) -> i32)(AGENT, p);
    }
}

unsafe fn take(p: *mut c_char) -> String {
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    dealloc(p);
    s
}

unsafe fn signature_of(klass: J) -> String {
    if klass == 0 {
        return "null".into();
    }
    let mut sig: *mut c_char = std::ptr::null_mut();
    if jvmti!(48, extern "system" fn(Env, J, *mut *mut c_char, *mut *mut c_char) -> i32)(
        AGENT,
        klass,
        &mut sig,
        std::ptr::null_mut(),
    ) != 0
    {
        return "?".into();
    }
    take(sig)
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

unsafe fn group_info(group: J, info: &mut GroupInfo) -> i32 {
    jvmti!(14, extern "system" fn(Env, J, *mut GroupInfo) -> i32)(AGENT, group, info)
}

unsafe fn group_name(group: J) -> String {
    if group == 0 {
        return "null".into();
    }
    let mut info: GroupInfo = std::mem::zeroed();
    if group_info(group, &mut info) != 0 {
        return "?".into();
    }
    take(info.name)
}

unsafe fn loader_kind(env: Env, loader: J, app: J) -> &'static str {
    if loader == 0 {
        return "null";
    }
    if jni!(env, 24, extern "system" fn(Env, J, J) -> u8)(env, loader, app) != 0 {
        "app"
    } else {
        "other"
    }
}

unsafe fn thread_info(thread: J, info: &mut ThreadInfo) -> i32 {
    jvmti!(9, extern "system" fn(Env, J, *mut ThreadInfo) -> i32)(AGENT, thread, info)
}

unsafe fn thread_row(out: &mut String, env: Env, row: &str, thread: J, app: J) {
    let mut info: ThreadInfo = std::mem::zeroed();
    let err = thread_info(thread, &mut info);
    if err != 0 {
        let _ = writeln!(out, "thread {row}: err={err}");
        return;
    }
    let group = group_name(info.thread_group);
    let _ = writeln!(
        out,
        "thread {row}: err={err} name={} prio={} daemon={} group={group} loader={}",
        take(info.name),
        info.priority,
        info.is_daemon,
        loader_kind(env, info.context_class_loader, app)
    );
}

unsafe fn group_row(out: &mut String, group: J) {
    let mut info: GroupInfo = std::mem::zeroed();
    let err = group_info(group, &mut info);
    if err != 0 {
        let _ = writeln!(out, "group: err={err}");
        return;
    }
    let parent = group_name(info.parent);
    let name = take(info.name);
    let _ = writeln!(
        out,
        "group {name}: err={err} name={name} parent={parent} max={} daemon={}",
        info.max_priority, info.is_daemon
    );
}

unsafe fn class_row(out: &mut String, env: Env, klass: J, app: J) {
    let sig = signature_of(klass);
    let mut modifiers: i32 = -1;
    let mut is_interface: u8 = 2;
    let mut is_array: u8 = 2;
    jvmti!(51, extern "system" fn(Env, J, *mut i32) -> i32)(AGENT, klass, &mut modifiers);
    jvmti!(55, extern "system" fn(Env, J, *mut u8) -> i32)(AGENT, klass, &mut is_interface);
    jvmti!(56, extern "system" fn(Env, J, *mut u8) -> i32)(AGENT, klass, &mut is_array);
    let mut source_name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(50, extern "system" fn(Env, J, *mut *mut c_char) -> i32)(AGENT, klass, &mut source_name);
    let source = if err == 0 { format!("0:{}", take(source_name)) } else { format!("{err}") };
    let mut loader: J = 0;
    jvmti!(57, extern "system" fn(Env, J, *mut J) -> i32)(AGENT, klass, &mut loader);
    let mut count: i32 = 0;
    let mut list: *mut J = std::ptr::null_mut();
    let err = jvmti!(54, extern "system" fn(Env, J, *mut i32, *mut *mut J) -> i32)(AGENT, klass, &mut count, &mut list);
    let interfaces = if err == 0 {
        let mut s = format!("{count}");
        for i in 0..count as usize {
            s.push(' ');
            s.push_str(&signature_of(*list.add(i)));
        }
        dealloc(list.cast());
        s
    } else {
        format!("err={err}")
    };
    let methods_err = {
        let mut n: i32 = 0;
        let mut m: *mut usize = std::ptr::null_mut();
        let e = jvmti!(52, extern "system" fn(Env, J, *mut i32, *mut *mut usize) -> i32)(AGENT, klass, &mut n, &mut m);
        dealloc(m.cast());
        e
    };
    let (mut minor, mut major) = (0i32, 0i32);
    let err = jvmti!(145, extern "system" fn(Env, J, *mut i32, *mut i32) -> i32)(AGENT, klass, &mut minor, &mut major);
    let version = if err == 0 { format!("0:{major}") } else { format!("{err}") };
    let _ = writeln!(
        out,
        "class {sig}: mods=0x{:x} interface={is_interface} array={is_array} source={source} loader={} interfaces={interfaces} methods={methods_err} version={version}",
        modifiers as u32,
        loader_kind(env, loader, app)
    );
}

unsafe fn field_row(out: &mut String, klass: J, field: usize) {
    let (mut name, mut sig, mut generic): (*mut c_char, *mut c_char, *mut c_char) =
        (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
    let err = jvmti!(60, extern "system" fn(Env, J, usize, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        AGENT, klass, field, &mut name, &mut sig, &mut generic,
    );
    if err != 0 {
        let _ = writeln!(out, "field: err={err}");
        return;
    }
    let mut modifiers: i32 = -1;
    let mut declaring: J = 0;
    let mut synthetic: u8 = 2;
    jvmti!(62, extern "system" fn(Env, J, usize, *mut i32) -> i32)(AGENT, klass, field, &mut modifiers);
    jvmti!(61, extern "system" fn(Env, J, usize, *mut J) -> i32)(AGENT, klass, field, &mut declaring);
    jvmti!(63, extern "system" fn(Env, J, usize, *mut u8) -> i32)(AGENT, klass, field, &mut synthetic);
    let declaring_sig = signature_of(declaring);
    let generic = if generic.is_null() { "-".to_string() } else { take(generic) };
    let _ = writeln!(
        out,
        "field {} {} generic={generic} mods=0x{:x} declaring={declaring_sig} synthetic={synthetic}",
        take(name),
        take(sig),
        modifiers as u32
    );
}

unsafe fn fields_rows(out: &mut String, row: &str, klass: J) {
    let mut count: i32 = 0;
    let mut fields: *mut usize = std::ptr::null_mut();
    let err = jvmti!(53, extern "system" fn(Env, J, *mut i32, *mut *mut usize) -> i32)(AGENT, klass, &mut count, &mut fields);
    let _ = writeln!(out, "fields of {row}: err={err} count={count}");
    if err == 0 {
        for i in 0..count as usize {
            field_row(out, klass, *fields.add(i));
        }
    }
    dealloc(fields.cast());
}

unsafe fn field_name_of(klass: J, field: usize) -> String {
    let mut name: *mut c_char = std::ptr::null_mut();
    let err = jvmti!(60, extern "system" fn(Env, J, usize, *mut *mut c_char, *mut *mut c_char, *mut *mut c_char) -> i32)(
        AGENT, klass, field, &mut name, std::ptr::null_mut(), std::ptr::null_mut(),
    );
    if err != 0 {
        return format!("err={err}");
    }
    take(name)
}

unsafe fn method_row(out: &mut String, env: Env, klass: J, name: &str, sig: &str, is_static: bool) {
    let n = CString::new(name).unwrap();
    let s = CString::new(sig).unwrap();
    let method: usize = if is_static {
        jni!(env, 113, extern "system" fn(Env, J, *const c_char, *const c_char) -> usize)(env, klass, n.as_ptr(), s.as_ptr())
    } else {
        jni!(env, 33, extern "system" fn(Env, J, *const c_char, *const c_char) -> usize)(env, klass, n.as_ptr(), s.as_ptr())
    };
    if method == 0 {
        jni!(env, 17, extern "system" fn(Env))(env);
        let _ = writeln!(out, "method {name}: missing");
        return;
    }
    {
        let mut count: i32 = -1;
        let mut codes: *mut u8 = std::ptr::null_mut();
        let err = jvmti!(75, extern "system" fn(Env, usize, *mut i32, *mut *mut u8) -> i32)(AGENT, method, &mut count, &mut codes);
        let mut hex = String::new();
        if err == 0 {
            for k in 0..(count.min(16) as usize) {
                hex.push_str(&format!("{:02x}", *codes.add(k)));
            }
            dealloc(codes.cast());
        }
        let _ = writeln!(out, "bytecodes {name}: err={err} count={count} {hex}");
    }
    let (mut max_locals, mut args_size, mut synthetic) = (-1i32, -1i32, 2u8);
    let e1 = jvmti!(68, extern "system" fn(Env, usize, *mut i32) -> i32)(AGENT, method, &mut max_locals);
    let e2 = jvmti!(69, extern "system" fn(Env, usize, *mut i32) -> i32)(AGENT, method, &mut args_size);
    let e3 = jvmti!(77, extern "system" fn(Env, usize, *mut u8) -> i32)(AGENT, method, &mut synthetic);
    let _ = writeln!(
        out,
        "method {name}: maxLocals={e1}:{max_locals} argsSize={e2}:{args_size} synthetic={e3}:{synthetic}"
    );
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W52JvmtiThreadAndClassFacts_rows(
    env: Env,
    _self: J,
    worker: J,
    unstarted: J,
    app: J,
    classes: J,
    pair: J,
    inner: J,
) -> J {
    let mut out = String::new();
    let new_string = jni!(env, 167, extern "system" fn(Env, *const c_char) -> J);
    if AGENT.is_null() {
        return new_string(env, b"no JVMTI env (load the shim with -agentpath too)\n\0".as_ptr().cast());
    }
    let (a, b, c) = ONLOAD;
    let _ = writeln!(out, "onload: caps err={a} monitor err={b} storage err={c} events err={}", ONLOAD_EVENTS);
    thread_row(&mut out, env, "NULL", 0, app);

    let mut phase: i32 = -1;
    let err = jvmti!(133, extern "system" fn(Env, *mut i32) -> i32)(AGENT, &mut phase);
    let _ = writeln!(out, "phase: err={err} phase={phase}");
    let mut storage: *const c_void = std::ptr::null();
    let err = jvmti!(147, extern "system" fn(Env, *mut *const c_void) -> i32)(AGENT, &mut storage);
    let _ = writeln!(
        out,
        "storage: err={err} same={}",
        i32::from(storage == std::ptr::addr_of!(COOKIE).cast())
    );

    {
        let m = MONITOR;
        let op = |n: usize| -> i32 {
            std::mem::transmute::<usize, extern "system" fn(Env, usize) -> i32>(slot(AGENT, n - 1))(AGENT, m)
        };
        let a = op(33);
        let b = op(33);
        let c = op(34);
        let d = jvmti!(35, extern "system" fn(Env, usize, i64) -> i32)(AGENT, m, 1);
        let e = op(36);
        let f = op(37);
        let g = op(34);
        let h = op(34);
        let k = op(32);
        let _ = writeln!(
            out,
            "raw monitor: enter={a} enter={b} exit={c} wait={d} notify={e} notifyAll={f} exit={g} exit={h} destroy={k}"
        );
    }

    {
        let mut mine: *const c_void = std::ptr::null();
        let mut theirs: *const c_void = 1 as *const c_void;
        let set = jvmti!(103, extern "system" fn(Env, J, *const c_void) -> i32)(AGENT, 0, std::ptr::addr_of!(COOKIE).cast());
        let get = jvmti!(102, extern "system" fn(Env, J, *mut *const c_void) -> i32)(AGENT, 0, &mut mine);
        let other = jvmti!(102, extern "system" fn(Env, J, *mut *const c_void) -> i32)(AGENT, worker, &mut theirs);
        let _ = writeln!(out, "thread storage: set={set} get={get} same={} worker={other} null={}",
            i32::from(mine == std::ptr::addr_of!(COOKIE).cast()), i32::from(theirs.is_null()));
    }
    thread_row(&mut out, env, "worker", worker, app);
    thread_row(&mut out, env, "unstarted", unstarted, app);

    let mut info: ThreadInfo = std::mem::zeroed();
    if thread_info(worker, &mut info) == 0 {
        dealloc(info.name);
        group_row(&mut out, info.thread_group);
        let mut group: GroupInfo = std::mem::zeroed();
        if group_info(info.thread_group, &mut group) == 0 {
            dealloc(group.name);
            group_row(&mut out, group.parent);
        }
    }

    let mut count: i32 = 0;
    let mut groups: *mut J = std::ptr::null_mut();
    let err = jvmti!(13, extern "system" fn(Env, *mut i32, *mut *mut J) -> i32)(AGENT, &mut count, &mut groups);
    if err == 0 && count > 0 {
        let mut top: GroupInfo = std::mem::zeroed();
        group_info(*groups, &mut top);
        let name = group_name(*groups);
        let parent = group_name(top.parent);
        dealloc(top.name);
        let _ = writeln!(out, "top groups: err={err} count={count} first={name} parent={parent}");
    } else {
        let _ = writeln!(out, "top groups: err={err} count={count}");
    }
    dealloc(groups.cast());

    let len = jni!(env, 171, extern "system" fn(Env, J) -> i32)(env, classes);
    for i in 0..len {
        let klass = jni!(env, 173, extern "system" fn(Env, J, i32) -> J)(env, classes, i);
        class_row(&mut out, env, klass, app);
    }

    fields_rows(&mut out, "Pair", pair);
    let created = jni!(env, 144, extern "system" fn(Env, J, *const c_char, *const c_char) -> usize)(
        env, pair, b"created\0".as_ptr().cast(), b"I\0".as_ptr().cast(),
    );
    let count_field = jni!(env, 94, extern "system" fn(Env, J, *const c_char, *const c_char) -> usize)(
        env, pair, b"count\0".as_ptr().cast(), b"I\0".as_ptr().cast(),
    );
    if created == 0 || count_field == 0 {
        jni!(env, 17, extern "system" fn(Env))(env);
        let _ = writeln!(out, "jni ids: missing");
    } else {
        let created_name = field_name_of(pair, created);
        let count_name = field_name_of(pair, count_field);
        let value = jni!(env, 150, extern "system" fn(Env, J, usize) -> i32)(env, pair, created);
        let _ = writeln!(
            out,
            "jni ids: static created={value} name={created_name} instance count name={count_name}"
        );
    }
    fields_rows(&mut out, "Inner", inner);

    method_row(&mut out, env, pair, "sum", "(IJLjava/lang/Object;)J", true);
    method_row(&mut out, env, pair, "run", "()V", false);

    thread_row(&mut out, env, "NULL again", 0, app);
    let text = CString::new(out).unwrap();
    new_string(env, text.as_ptr())
}
