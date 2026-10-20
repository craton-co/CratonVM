//! Rust port of tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.c: the same
//! calls in the same order, the same lines.
#![allow(non_snake_case, static_mut_refs, clippy::missing_safety_doc)]

#[path = "../../common/agent.rs"]
#[macro_use]
mod agent;

use agent::{Env, J};
use std::ffi::{c_char, c_void};

const NO_ENV: &str = "no JVMTI env (load the shim with -agentpath too)";

static mut AGENT: Env = std::ptr::null_mut();

#[no_mangle]
pub unsafe extern "system" fn Agent_OnLoad(vm: *mut *const usize, _options: *mut c_char, _r: *mut c_void) -> i32 {
    let Some(jvmti) = agent::get_env(vm, agent::JVMTI_VERSION_1_2) else {
        return 1;
    };
    AGENT = jvmti;
    // can_tag_objects only.
    let caps: [u32; 4] = [1, 0, 0, 0];
    jvmti!(jvmti, 142, extern "system" fn(Env, *const [u32; 4]) -> i32)(jvmti, &caps);
    0
}

macro_rules! need_agent {
    ($env:expr) => {
        if AGENT.is_null() {
            return agent::new_string($env, &format!("{NO_ENV}\n"));
        }
    };
}

type GetTag = extern "system" fn(Env, J, *mut i64) -> i32;
type SetTag = extern "system" fn(Env, J, i64) -> i32;
type WithTags = extern "system" fn(Env, i32, *const i64, *mut i32, *mut *mut J, *mut *mut i64) -> i32;

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_getTag(_env: Env, _cls: J, o: J) -> i64 {
    let mut tag: i64 = 0;
    let e = jvmti!(AGENT, 106, GetTag)(AGENT, o, &mut tag);
    if e != 0 {
        -i64::from(e)
    } else {
        tag
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_setTag(_env: Env, _cls: J, o: J, tag: i64) -> i32 {
    jvmti!(AGENT, 107, SetTag)(AGENT, o, tag)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_tagErrors(env: Env, _cls: J, any: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let mut tag: i64 = 0;
    let mut n: i32 = 0;
    let mut objs: *mut J = std::ptr::null_mut();
    let one = [1i64];
    let zero = [0i64];
    let rows: [(&str, i32); 7] = [
        ("GetTag null object", jvmti!(jvmti, 106, GetTag)(jvmti, 0, &mut tag)),
        ("GetTag null tag_ptr", jvmti!(jvmti, 106, GetTag)(jvmti, any, std::ptr::null_mut())),
        ("SetTag null object", jvmti!(jvmti, 107, SetTag)(jvmti, 0, 1)),
        ("GetObjectsWithTags negative", jvmti!(jvmti, 114, WithTags)(jvmti, -1, one.as_ptr(), &mut n, &mut objs, std::ptr::null_mut())),
        ("GetObjectsWithTags null tags", jvmti!(jvmti, 114, WithTags)(jvmti, 1, std::ptr::null(), &mut n, &mut objs, std::ptr::null_mut())),
        ("GetObjectsWithTags tag 0", jvmti!(jvmti, 114, WithTags)(jvmti, 1, zero.as_ptr(), &mut n, &mut objs, std::ptr::null_mut())),
        ("GetObjectsWithTags null count", jvmti!(jvmti, 114, WithTags)(jvmti, 1, one.as_ptr(), std::ptr::null_mut(), &mut objs, std::ptr::null_mut())),
    ];
    let mut out = String::new();
    for (row, e) in rows {
        out.push_str(&format!("{row}: err={e}\n"));
    }
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_objectsWithTags(env: Env, _cls: J, tags: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let n = jni!(env, 171, extern "system" fn(Env, J) -> i32)(env, tags);
    let elems = jni!(env, 188, extern "system" fn(Env, J, *mut u8) -> *mut i64)(env, tags, std::ptr::null_mut());
    let mut count: i32 = 0;
    let mut objs: *mut J = std::ptr::null_mut();
    let mut found: *mut i64 = std::ptr::null_mut();
    let e = jvmti!(jvmti, 114, WithTags)(jvmti, n, elems, &mut count, &mut objs, &mut found);
    jni!(env, 196, extern "system" fn(Env, J, *mut i64, i32))(env, tags, elems, 2);
    if e != 0 {
        return agent::new_string(env, &format!("err={e}"));
    }
    let mut list: Vec<i64> = (0..count.max(0) as usize).map(|i| *found.add(i)).collect();
    list.sort_unstable();
    let objects_non_null = (0..count.max(0) as usize).all(|i| *objs.add(i) != 0);
    agent::deallocate(jvmti, objs.cast());
    agent::deallocate(jvmti, found.cast());
    let list: Vec<String> = list.iter().map(i64::to_string).collect();
    agent::new_string(env, &format!("err=0 count={count} objects={} tags=[{}]", u8::from(objects_non_null), list.join(",")))
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_forceGc(_env: Env, _cls: J) -> i32 {
    jvmti!(AGENT, 108, extern "system" fn(Env) -> i32)(AGENT)
}

// -------------------------------------------------------------------------
// IterateThroughHeap

const VISIT_OBJECTS: i32 = 0x100;
const VISIT_ABORT: i32 = 0x8000;

#[repr(C)]
#[derive(Default)]
struct HeapCallbacks {
    heap_iteration: usize,
    heap_reference: usize,
    primitive_field: usize,
    array_primitive_value: usize,
    string_primitive_value: usize,
    reserved: [usize; 10],
}

#[derive(Default)]
struct Seen {
    count: i32,
    tagged: i32,
    sizes: Vec<i64>,
    lengths: Vec<i32>,
    class_tags: Vec<i64>,
    retag: i64,
    abort_after: i32,
    want: i64,
    rows: Vec<String>,
}

unsafe extern "system" fn iteration_cb(class_tag: i64, size: i64, tag_ptr: *mut i64, length: i32, user: *mut c_void) -> i32 {
    let seen = &mut *(user as *mut Seen);
    seen.count += 1;
    if *tag_ptr != 0 {
        seen.tagged += 1;
    }
    if !seen.sizes.contains(&size) {
        seen.sizes.push(size);
    }
    if !seen.lengths.contains(&length) {
        seen.lengths.push(length);
    }
    if !seen.class_tags.contains(&class_tag) {
        seen.class_tags.push(class_tag);
    }
    if seen.retag != 0 {
        *tag_ptr = seen.retag;
    }
    if seen.abort_after != 0 && seen.count >= seen.abort_after {
        return VISIT_ABORT;
    }
    0
}

type ThroughHeap = extern "system" fn(Env, i32, J, *const HeapCallbacks, *const c_void) -> i32;

fn sorted<T: Ord + ToString + Copy>(v: &[T]) -> String {
    let mut v = v.to_vec();
    v.sort_unstable();
    v.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_throughHeap(
    env: Env,
    _cls: J,
    filter: i32,
    klass: J,
    retag: i64,
    abort_after: i32,
) -> J {
    need_agent!(env);
    let callbacks = HeapCallbacks {
        heap_iteration: iteration_cb as *const () as usize,
        ..Default::default()
    };
    let mut seen = Seen {
        retag,
        abort_after,
        ..Default::default()
    };
    let e = jvmti!(AGENT, 116, ThroughHeap)(AGENT, filter, klass, &callbacks, (&mut seen as *mut Seen).cast());
    agent::new_string(
        env,
        &format!(
            "err={e} count={} tagged={} sizes=[{}] lengths=[{}] class-tags=[{}]",
            seen.count,
            seen.tagged,
            sorted(&seen.sizes),
            sorted(&seen.lengths),
            sorted(&seen.class_tags)
        ),
    )
}

unsafe extern "system" fn array_cb(
    _class_tag: i64,
    size: i64,
    tag_ptr: *mut i64,
    count: i32,
    element_type: i32,
    elements: *const c_void,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    if *tag_ptr == seen.want {
        let mut sum: i64 = 0;
        if element_type == 73 {
            for i in 0..count.max(0) as usize {
                sum += i64::from(*(elements as *const i32).add(i));
            }
        }
        seen.rows.push(format!("array size={size} count={count} type={element_type} sum={sum}"));
    }
    0
}

unsafe extern "system" fn string_cb(
    _class_tag: i64,
    size: i64,
    tag_ptr: *mut i64,
    value: *const u16,
    length: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    if *tag_ptr == seen.want {
        let chars = std::slice::from_raw_parts(value, length.max(0) as usize);
        seen.rows.push(format!("string size={size} length={length} value={}", String::from_utf16_lossy(chars)));
    }
    0
}

/// `jvmtiHeapReferenceInfo`, read as its first `jint` (the field, array
/// and constant pool members' `index`).
#[repr(C)]
struct ReferenceInfo {
    index: i32,
}

unsafe extern "system" fn field_cb(
    kind: i32,
    info: *const ReferenceInfo,
    _class_tag: i64,
    tag_ptr: *mut i64,
    value: u64,
    value_type: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    if *tag_ptr == seen.want {
        let shown = match value_type {
            90 => (value as u8 != 0).to_string(),
            66 => (value as u8 as i8).to_string(),
            67 => (value as u16).to_string(),
            83 => (value as u16 as i16).to_string(),
            73 => (value as u32 as i32).to_string(),
            74 => (value as i64).to_string(),
            70 => f32::from_bits(value as u32).to_string(),
            _ => f64::from_bits(value).to_string(),
        };
        seen.rows.push(format!("field kind={kind} index={} type={value_type} value={shown}", (*info).index));
    }
    0
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_primitives(env: Env, _cls: J, klass: J, tag: i64) -> J {
    need_agent!(env);
    let callbacks = HeapCallbacks {
        primitive_field: field_cb as *const () as usize,
        array_primitive_value: array_cb as *const () as usize,
        string_primitive_value: string_cb as *const () as usize,
        ..Default::default()
    };
    let mut seen = Seen {
        want: tag,
        ..Default::default()
    };
    let e = jvmti!(AGENT, 116, ThroughHeap)(AGENT, 0, klass, &callbacks, (&mut seen as *mut Seen).cast());
    seen.rows.sort();
    agent::new_string(env, &format!("err={e} [{}]", seen.rows.join("; ")))
}

// -------------------------------------------------------------------------
// FollowReferences

type Follow = extern "system" fn(Env, i32, J, J, *const HeapCallbacks, *const c_void) -> i32;

unsafe extern "system" fn reference_cb(
    kind: i32,
    info: *const ReferenceInfo,
    _class_tag: i64,
    _referrer_class_tag: i64,
    size: i64,
    tag_ptr: *mut i64,
    referrer_tag_ptr: *mut i64,
    length: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    seen.count += 1;
    let tag = *tag_ptr;
    let referrer = if referrer_tag_ptr.is_null() { -1 } else { *referrer_tag_ptr };
    if tag != 0 {
        let index = match kind {
            2 | 3 | 8 | 9 => (*info).index.to_string(),
            _ => "-".into(),
        };
        seen.rows.push(format!("kind={kind} {referrer}->{tag} index={index} size={size} length={length}"));
    }
    if seen.abort_after != 0 && seen.count >= seen.abort_after {
        return VISIT_ABORT;
    }
    if tag != 0 {
        VISIT_OBJECTS
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_follow(env: Env, _cls: J, klass: J, initial: J, abort_after: i32) -> J {
    need_agent!(env);
    let callbacks = HeapCallbacks {
        heap_reference: reference_cb as *const () as usize,
        ..Default::default()
    };
    let mut seen = Seen {
        abort_after,
        ..Default::default()
    };
    let e = jvmti!(AGENT, 115, Follow)(AGENT, 0, klass, initial, &callbacks, (&mut seen as *mut Seen).cast());
    seen.rows.sort();
    let mut out = format!(
        "follow klass={} abort={abort_after}: err={e} calls-to-tagged={}\n",
        if klass == 0 { "null" } else { "set" },
        seen.rows.len()
    );
    if abort_after != 0 {
        out = format!("follow abort={abort_after}: err={e} calls={}\n", seen.count);
        return agent::new_string(env, &out);
    }
    for row in &seen.rows {
        out.push_str(&format!("  {row}\n"));
    }
    agent::new_string(env, &out)
}

unsafe extern "system" fn roots_cb(
    kind: i32,
    _info: *const ReferenceInfo,
    _class_tag: i64,
    _referrer_class_tag: i64,
    _size: i64,
    tag_ptr: *mut i64,
    _referrer_tag_ptr: *mut i64,
    _length: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    let tag = *tag_ptr;
    if (10..=15).contains(&tag) {
        if !seen.class_tags.contains(&tag) {
            seen.class_tags.push(tag);
        }
        if tag == 10 && !seen.lengths.contains(&kind) {
            seen.lengths.push(kind);
        }
    }
    VISIT_OBJECTS
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_followRoots(env: Env, _cls: J, klass: J) -> J {
    need_agent!(env);
    let callbacks = HeapCallbacks {
        heap_reference: roots_cb as *const () as usize,
        ..Default::default()
    };
    let mut seen = Seen::default();
    let e = jvmti!(AGENT, 115, Follow)(AGENT, 0, klass, 0, &callbacks, (&mut seen as *mut Seen).cast());
    agent::new_string(
        env,
        &format!(
            "follow roots: err={e} tags-reached=[{}] kinds-to-root-node=[{}]\n",
            sorted(&seen.class_tags),
            sorted(&seen.lengths)
        ),
    )
}

// -------------------------------------------------------------------------
// JVMTI 1.0 walks

unsafe extern "system" fn old_object_cb(_class_tag: i64, size: i64, tag_ptr: *mut i64, user: *mut c_void) -> i32 {
    let seen = &mut *(user as *mut Seen);
    seen.count += 1;
    if *tag_ptr != 0 {
        seen.tagged += 1;
    }
    if !seen.sizes.contains(&size) {
        seen.sizes.push(size);
    }
    1
}

unsafe extern "system" fn old_reference_cb(
    kind: i32,
    _class_tag: i64,
    _size: i64,
    tag_ptr: *mut i64,
    referrer_tag: i64,
    referrer_index: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    let tag = *tag_ptr;
    if tag != 0 {
        seen.rows.push(format!("kind={kind} {referrer_tag}->{tag} index={referrer_index}"));
        1
    } else {
        2
    }
}

unsafe extern "system" fn old_root_cb(_kind: i32, _class_tag: i64, _size: i64, tag_ptr: *mut i64, user: *mut c_void) -> i32 {
    let seen = &mut *(user as *mut Seen);
    let tag = *tag_ptr;
    if tag != 0 && !seen.class_tags.contains(&tag) {
        seen.class_tags.push(tag);
    }
    1
}

unsafe extern "system" fn old_stack_cb(
    _kind: i32,
    _class_tag: i64,
    _size: i64,
    tag_ptr: *mut i64,
    _thread_tag: i64,
    _depth: i32,
    _method: J,
    _slot: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    let tag = *tag_ptr;
    if tag != 0 && !seen.class_tags.contains(&tag) {
        seen.class_tags.push(tag);
    }
    1
}

unsafe extern "system" fn old_reach_cb(
    _kind: i32,
    _class_tag: i64,
    _size: i64,
    tag_ptr: *mut i64,
    _referrer_tag: i64,
    _referrer_index: i32,
    user: *mut c_void,
) -> i32 {
    let seen = &mut *(user as *mut Seen);
    let tag = *tag_ptr;
    if (10..=15).contains(&tag) && !seen.class_tags.contains(&tag) {
        seen.class_tags.push(tag);
    }
    1
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_oldWalks(env: Env, _cls: J, klass: J, initial: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let mut out = String::new();
    let mut seen = Seen::default();
    let e = jvmti!(jvmti, 111, extern "system" fn(Env, i32, usize, *mut c_void) -> i32)(
        jvmti,
        1,
        old_object_cb as *const () as usize,
        (&mut seen as *mut Seen).cast(),
    );
    out.push_str(&format!("IterateOverHeap tagged: err={e} count-at-least-7={}\n", u8::from(seen.count >= 7)));
    let mut seen = Seen::default();
    let e = jvmti!(jvmti, 112, extern "system" fn(Env, J, i32, usize, *mut c_void) -> i32)(
        jvmti,
        klass,
        3,
        old_object_cb as *const () as usize,
        (&mut seen as *mut Seen).cast(),
    );
    out.push_str(&format!(
        "IterateOverInstancesOfClass Node: err={e} count={} tagged={} sizes=[{}]\n",
        seen.count,
        seen.tagged,
        sorted(&seen.sizes)
    ));
    let mut seen = Seen::default();
    let e = jvmti!(jvmti, 109, extern "system" fn(Env, J, usize, *mut c_void) -> i32)(
        jvmti,
        initial,
        old_reference_cb as *const () as usize,
        (&mut seen as *mut Seen).cast(),
    );
    seen.rows.sort();
    out.push_str(&format!("IterateOverObjectsReachableFromObject: err={e}\n"));
    for row in &seen.rows {
        out.push_str(&format!("  {row}\n"));
    }
    let mut seen = Seen::default();
    let e = jvmti!(jvmti, 110, extern "system" fn(Env, usize, usize, usize, *mut c_void) -> i32)(
        jvmti,
        old_root_cb as *const () as usize,
        old_stack_cb as *const () as usize,
        old_reach_cb as *const () as usize,
        (&mut seen as *mut Seen).cast(),
    );
    seen.class_tags.retain(|t| (10..=15).contains(t));
    out.push_str(&format!("IterateOverReachableObjects: err={e} tags-reached=[{}]\n", sorted(&seen.class_tags)));
    agent::new_string(env, &out)
}

#[no_mangle]
pub unsafe extern "system" fn Java_L1W63JvmtiTagsAndHeap_walkErrors(env: Env, _cls: J, klass: J, any: J) -> J {
    need_agent!(env);
    let jvmti = AGENT;
    let callbacks = HeapCallbacks::default();
    let cb = old_object_cb as *const () as usize;
    let mut seen = Seen::default();
    let user: *mut c_void = (&mut seen as *mut Seen).cast();
    let rows: [(&str, i32); 8] = [
        ("IterateThroughHeap null callbacks", jvmti!(jvmti, 116, ThroughHeap)(jvmti, 0, klass, std::ptr::null(), std::ptr::null())),
        ("IterateThroughHeap object as klass", jvmti!(jvmti, 116, ThroughHeap)(jvmti, 0, any, &callbacks, std::ptr::null())),
        ("FollowReferences null callbacks", jvmti!(jvmti, 115, Follow)(jvmti, 0, klass, 0, std::ptr::null(), std::ptr::null())),
        ("FollowReferences object as klass", jvmti!(jvmti, 115, Follow)(jvmti, 0, any, 0, &callbacks, std::ptr::null())),
        ("IterateOverHeap null callback", jvmti!(jvmti, 111, extern "system" fn(Env, i32, usize, *mut c_void) -> i32)(jvmti, 3, 0, std::ptr::null_mut())),
        ("IterateOverHeap filter 9", jvmti!(jvmti, 111, extern "system" fn(Env, i32, usize, *mut c_void) -> i32)(jvmti, 9, cb, user)),
        ("IterateOverInstancesOfClass null klass", jvmti!(jvmti, 112, extern "system" fn(Env, J, i32, usize, *mut c_void) -> i32)(jvmti, 0, 3, cb, user)),
        ("IterateOverObjectsReachableFromObject null object", jvmti!(jvmti, 109, extern "system" fn(Env, J, usize, *mut c_void) -> i32)(jvmti, 0, cb, user)),
    ];
    let mut out = String::new();
    for (row, e) in rows {
        out.push_str(&format!("{row}: err={e}\n"));
    }
    agent::new_string(env, &out)
}
