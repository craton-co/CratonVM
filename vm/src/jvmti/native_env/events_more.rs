// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The events of the C table the i51-L1 page's "Next step (from wave 61)"
//! listed (interpreter round i2 wave 63, lane L1), each posted where the VM
//! does the work, behind its bit of `DebuggerGates::native_event_armed`
//! ([`refresh_event_gates`]), with HotSpot 25's arguments and order
//! (MEASURED, `tools/probes/interp/L1/L1W63JvmtiEvents.java`):
//!
//! * `FieldAccess` / `FieldModification` (63 / 64) of the fields
//!   `SetField*Watch` names, from the field bytecodes' watch hook
//!   (`interpreter::deliver_field_watch_if_armed`), which the watches put
//!   every method under the interpreter for, as a JDWP watch does;
//! * the monitor events (73-76), from `Object.wait` and the contended
//!   `monitorenter` (`vm_exec::monitor_enter_blocking`);
//! * `GarbageCollectionStart` / `Finish` (81 / 82), from the collectors'
//!   hooks, in the pause (the callback may call only the raw monitor
//!   functions, as on HotSpot);
//! * `ObjectFree` (83) and the `ClassUnload` extension event (49), after a
//!   collection, from a service thread ([`service_thread`]), as HotSpot's
//!   service thread posts them;
//! * `VMObjectAlloc` (84) of the objects the VM allocates for the program:
//!   `Object.clone`, `Array.newInstance`, a reflective constructor
//!   (MEASURED: not JNI's `AllocObject` / `NewObject`, nor a bytecode `new`);
//! * `SampledObjectAlloc` (86) from the allocation funnels, every
//!   `SetHeapSamplingInterval` bytes on average (every allocation for 0);
//! * `NativeMethodBind` (67) where a JNI native is bound (a library symbol,
//!   `RegisterNatives`), the address the callback hands back bound instead;
//! * `ResourceExhausted` (80) where the VM raises an `OutOfMemoryError`,
//!   with HotSpot's `[error][jvmti]` line;
//! * `VirtualThreadStart` / `End` (87 / 88) and the mount extension events
//!   (48 / 47) at a virtual thread's mounts;
//! * `CompiledMethodLoad` / `Unload` (68 / 69) and `DynamicCodeGenerated`
//!   (70) from the JIT's code publication (`cratonvm_jit::code_events`), and
//!   `GenerateEvents` (123);
//! * `DataDumpRequest` (71) at `SIGQUIT`.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;
use crate::debug::NativeEvent;

pub(super) const EVENT_FIELD_ACCESS: JInt = 63;
pub(super) const EVENT_FIELD_MODIFICATION: JInt = 64;
pub(super) const EVENT_NATIVE_METHOD_BIND: JInt = 67;
pub(super) const EVENT_COMPILED_METHOD_LOAD: JInt = 68;
pub(super) const EVENT_COMPILED_METHOD_UNLOAD: JInt = 69;
pub(super) const EVENT_DYNAMIC_CODE_GENERATED: JInt = 70;
pub(super) const EVENT_DATA_DUMP_REQUEST: JInt = 71;
pub(super) const EVENT_MONITOR_WAIT: JInt = 73;
pub(super) const EVENT_MONITOR_WAITED: JInt = 74;
pub(super) const EVENT_MONITOR_CONTENDED_ENTER: JInt = 75;
pub(super) const EVENT_MONITOR_CONTENDED_ENTERED: JInt = 76;
pub(super) const EVENT_RESOURCE_EXHAUSTED: JInt = 80;
pub(super) const EVENT_GC_START: JInt = 81;
pub(super) const EVENT_GC_FINISH: JInt = 82;
pub(super) const EVENT_OBJECT_FREE: JInt = 83;
pub(super) const EVENT_VM_OBJECT_ALLOC: JInt = 84;
pub(super) const EVENT_SAMPLED_OBJECT_ALLOC: JInt = 86;
pub(super) const EVENT_VIRTUAL_THREAD_START: JInt = 87;
pub(super) const EVENT_VIRTUAL_THREAD_END: JInt = 88;

/// The capability an event of this module needs (all zero: none); `None`
/// for an event no part of the table delivers.
pub(super) fn event_capability(event: JInt) -> Option<[u32; CAPABILITY_WORDS]> {
    let n = match event {
        EVENT_FIELD_ACCESS => CAP_FIELD_ACCESS_EVENTS,
        EVENT_FIELD_MODIFICATION => CAP_FIELD_MODIFICATION_EVENTS,
        EVENT_NATIVE_METHOD_BIND => CAP_NATIVE_METHOD_BIND_EVENTS,
        EVENT_COMPILED_METHOD_LOAD | EVENT_COMPILED_METHOD_UNLOAD => {
            CAP_COMPILED_METHOD_LOAD_EVENTS
        }
        EVENT_MONITOR_WAIT
        | EVENT_MONITOR_WAITED
        | EVENT_MONITOR_CONTENDED_ENTER
        | EVENT_MONITOR_CONTENDED_ENTERED => CAP_MONITOR_EVENTS,
        EVENT_GC_START | EVENT_GC_FINISH => CAP_GARBAGE_COLLECTION_EVENTS,
        EVENT_OBJECT_FREE => CAP_OBJECT_FREE_EVENTS,
        EVENT_VM_OBJECT_ALLOC => CAP_VM_OBJECT_ALLOC_EVENTS,
        EVENT_SAMPLED_OBJECT_ALLOC => CAP_SAMPLED_OBJECT_ALLOC_EVENTS,
        EVENT_VIRTUAL_THREAD_START
        | EVENT_VIRTUAL_THREAD_END
        | extensions::EXT_EVENT_VIRTUAL_THREAD_MOUNT
        | extensions::EXT_EVENT_VIRTUAL_THREAD_UNMOUNT => CAP_SUPPORT_VIRTUAL_THREADS,
        EVENT_DYNAMIC_CODE_GENERATED
        | EVENT_DATA_DUMP_REQUEST
        | EVENT_RESOURCE_EXHAUSTED
        | extensions::EXT_EVENT_CLASS_UNLOAD => return Some([0; CAPABILITY_WORDS]),
        _ => return None,
    };
    Some(cap(n))
}

/// The events of this module JVMTI does not let an agent enable for one
/// thread (its list: `VirtualThreadStart`, `CompiledMethodLoad` /
/// `Unload`, `DynamicCodeGenerated`, `DataDumpRequest`), answered
/// `ILLEGAL_ARGUMENT` as HotSpot answers the global events.
pub(super) fn is_global_event(event: JInt) -> bool {
    matches!(
        event,
        EVENT_VIRTUAL_THREAD_START
            | EVENT_COMPILED_METHOD_LOAD
            | EVENT_COMPILED_METHOD_UNLOAD
            | EVENT_DYNAMIC_CODE_GENERATED
            | EVENT_DATA_DUMP_REQUEST
    )
}

// --- Gates and the VMs that listen -------------------------------------------

/// The VMs one of whose C envs listens for an event posted from a place
/// that has no VM in hand (the collectors' hooks, the JIT's code
/// publication, `SIGQUIT`, the service thread), with their masks. Empty
/// unless such an event is enabled; every reader first asks
/// [`GLOBAL_ARMED`].
static ARMED_VMS: parking_lot::Mutex<Vec<(usize, Weak<SharedVm>, u32)>> =
    parking_lot::Mutex::new(Vec::new());
/// The union of the masks of [`ARMED_VMS`]: one relaxed load in the
/// collectors' hooks and the JIT's publication.
static GLOBAL_ARMED: AtomicU32 = AtomicU32::new(0);

/// The events posted from places with no VM in hand.
const GLOBAL_MASK: u32 = NativeEvent::Gc as u32
    | NativeEvent::ObjectFree as u32
    | NativeEvent::ClassUnload as u32
    | NativeEvent::CompiledCode as u32
    | NativeEvent::DataDump as u32;

/// Raise or lower `shared`'s per-VM gates of the events this module posts,
/// from its envs' state; republish the debugger gates when the field watch
/// gate moves (it puts every method under the interpreter).
pub(super) fn refresh_event_gates(shared: &SharedVm) {
    refresh_class_prepare_gate(shared);
    let mask = {
        let jvmti = shared.debug.jvmti_env.lock();
        let any = |events: &[JInt]| {
            jvmti.c_table.native_envs.iter().any(|&addr| {
                // SAFETY: the list holds only addresses `new_env_for_version`
                // handed out.
                let state = unsafe { env_at(addr) }.state.lock();
                !state.disposed && events.iter().any(|&e| state.enabled_anywhere(e))
            })
        };
        let mut mask = 0u32;
        let watches = !jvmti.c_field_access_watches.is_empty()
            || !jvmti.c_field_modification_watches.is_empty();
        let mut set = |on: bool, bit: NativeEvent| {
            if on {
                mask |= bit as u32;
            }
        };
        set(
            watches && any(&[EVENT_FIELD_ACCESS, EVENT_FIELD_MODIFICATION]),
            NativeEvent::FieldWatch,
        );
        set(
            any(&[
                EVENT_MONITOR_WAIT,
                EVENT_MONITOR_WAITED,
                EVENT_MONITOR_CONTENDED_ENTER,
                EVENT_MONITOR_CONTENDED_ENTERED,
            ]),
            NativeEvent::Monitor,
        );
        set(any(&[EVENT_VM_OBJECT_ALLOC]), NativeEvent::VmObjectAlloc);
        set(
            any(&[EVENT_SAMPLED_OBJECT_ALLOC]),
            NativeEvent::SampledAlloc,
        );
        set(any(&[EVENT_NATIVE_METHOD_BIND]), NativeEvent::NativeBind);
        set(
            any(&[EVENT_RESOURCE_EXHAUSTED]),
            NativeEvent::ResourceExhausted,
        );
        set(
            any(&[
                EVENT_VIRTUAL_THREAD_START,
                EVENT_VIRTUAL_THREAD_END,
                extensions::EXT_EVENT_VIRTUAL_THREAD_MOUNT,
                extensions::EXT_EVENT_VIRTUAL_THREAD_UNMOUNT,
            ]),
            NativeEvent::VirtualThread,
        );
        set(
            any(&[extensions::EXT_EVENT_CLASS_UNLOAD]),
            NativeEvent::ClassUnload,
        );
        set(
            any(&[
                EVENT_COMPILED_METHOD_LOAD,
                EVENT_COMPILED_METHOD_UNLOAD,
                EVENT_DYNAMIC_CODE_GENERATED,
            ]),
            NativeEvent::CompiledCode,
        );
        set(any(&[EVENT_DATA_DUMP_REQUEST]), NativeEvent::DataDump);
        set(any(&[EVENT_GC_START, EVENT_GC_FINISH]), NativeEvent::Gc);
        set(any(&[EVENT_OBJECT_FREE]), NativeEvent::ObjectFree);
        mask
    };
    let gates = &shared.debug.debugger_gates;
    let watch_before = gates.native_event_armed(NativeEvent::FieldWatch);
    let sampled_before = gates.native_event_armed(NativeEvent::SampledAlloc);
    gates.set_native_events(mask);
    // Wave 83: the funnel's `NativeMethodBind` of the registered natives.
    let rust_binds = {
        let mut jvmti = shared.debug.jvmti_env.lock();
        let startup = jvmti.rust_native_bind_startup;
        let (next, armed) = rust_bind_tracking_step(
            jvmti.rust_native_bind_tracking,
            mask & NativeEvent::NativeBind as u32 != 0,
            startup,
        );
        jvmti.rust_native_bind_tracking = next;
        armed
    };
    gates.set_native_bind_hook(rust_binds);
    if watch_before != (mask & NativeEvent::FieldWatch as u32 != 0)
        || sampled_before != (mask & NativeEvent::SampledAlloc as u32 != 0)
    {
        let ds = shared.debug.debug_state.lock();
        crate::debug::publish_debugger_gates(shared, &ds);
    }
    {
        let mut vms = ARMED_VMS.lock();
        vms.retain(|(id, weak, _)| *id != shared.vm_identity && weak.strong_count() > 0);
        if mask & GLOBAL_MASK != 0 {
            if let Some(arc) = shared.try_get_arc() {
                vms.push((shared.vm_identity, Arc::downgrade(&arc), mask & GLOBAL_MASK));
            }
        }
        let union = vms.iter().fold(0, |acc, &(_, _, m)| acc | m);
        GLOBAL_ARMED.store(union, Ordering::Release);
        install_code_sink(union & NativeEvent::CompiledCode as u32 != 0);
    }
    if mask & (NativeEvent::ObjectFree as u32 | NativeEvent::ClassUnload as u32) != 0 {
        start_service_thread();
    }
}

/// The VMs armed for `bit`.
fn armed_vms(bit: NativeEvent) -> Vec<Arc<SharedVm>> {
    if GLOBAL_ARMED.load(Ordering::Relaxed) & bit as u32 == 0 {
        return Vec::new();
    }
    ARMED_VMS
        .lock()
        .iter()
        .filter(|(_, _, m)| m & bit as u32 != 0)
        .filter_map(|(_, weak, _)| weak.upgrade())
        .collect()
}

/// The callbacks of `event` of every env of `shared` that has it enabled for
/// thread `tid` (0: a global event): `(env address, callback)`.
fn callbacks(shared: &SharedVm, event: JInt, tid: u64) -> Vec<(usize, usize)> {
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    envs.into_iter()
        .filter_map(|env| callback_for(env, event, tid).map(|cb| (env, cb)))
        .collect()
}

/// [`callbacks`] for one of HotSpot's extension events.
fn ext_callbacks(shared: &SharedVm, index: JInt, tid: u64) -> Vec<(usize, usize)> {
    let envs = shared.debug.jvmti_env.lock().c_table.native_envs.clone();
    envs.into_iter()
        .filter_map(|env| extensions::ext_callback_for(env, index, tid).map(|cb| (env, cb)))
        .collect()
}

/// Run `call` for each `(env, callback)` in the event context of thread
/// `tid` (see [`in_event_context`]) with `obj` as a local reference; answers
/// where `obj` is after the callbacks, which may have collected.
fn with_object_callbacks(
    shared: &SharedVm,
    tid: u64,
    obj: ObjectRef,
    list: &[(usize, usize)],
    call: impl Fn(usize, usize, JNIEnv, JObject, JObject),
) -> ObjectRef {
    let mut current = obj;
    for &(env, cb) in list {
        in_event_context(shared, tid, |jni_env, thread| {
            let handle = jni::new_local_handle(current);
            call(env, cb, jni_env, thread, handle);
            if let Some(moved) = jni::jobject_to_obj(handle) {
                current = moved;
            }
        });
    }
    current
}

// --- ObjectFree (83) -------------------------------------------------------

/// `void (JNICALL *ObjectFree)(jvmtiEnv*, jlong tag)`.
type ObjectFreeCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, i64);

/// `ObjectFree` of each of `tags` to the env at `env_addr`, when it holds
/// `can_generate_object_free_events` and has the event enabled; with no
/// `JNIEnv` (the callback may call no JNI function).
pub(super) fn post_object_free(env_addr: usize, tags: &[i64]) {
    // SAFETY: an address `new_env_for_version` handed out.
    let env = unsafe { env_at(env_addr) };
    let callback = {
        let state = env.state.lock();
        if state.disposed
            || !has_cap(&state.capabilities, CAP_OBJECT_FREE_EVENTS)
            || !state.enabled_for(EVENT_OBJECT_FREE, 0)
        {
            return;
        }
        let slot = (EVENT_OBJECT_FREE - MIN_EVENT) as usize; // Cast: 33
        state.callbacks.get(slot).copied().filter(|&f| f != 0)
    };
    let Some(callback) = callback else {
        return;
    };
    // SAFETY: the agent's `ObjectFree` callback.
    let cb: ObjectFreeCallback = unsafe { std::mem::transmute(callback) };
    for &tag in tags {
        // SAFETY: a call of the agent's callback.
        unsafe { cb(env_addr as *mut JvmtiNativeEnv, tag) };
    }
}

// --- The service thread (ObjectFree, ClassUnload) ----------------------------

/// The work the service thread has: a collection finished, or classes were
/// unloaded (their VM, their internal names).
struct ServiceWork {
    collected: bool,
    unloaded: Vec<(usize, String)>,
}

static SERVICE: parking_lot::Mutex<ServiceWork> = parking_lot::Mutex::new(ServiceWork {
    collected: false,
    unloaded: Vec::new(),
});
static SERVICE_WAKE: parking_lot::Condvar = parking_lot::Condvar::new();
static SERVICE_STARTED: AtomicBool = AtomicBool::new(false);

/// Start [`service_thread`], once per process.
fn start_service_thread() {
    if SERVICE_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("jvmti-service".into())
        .spawn(service_thread);
    if spawned.is_err() {
        SERVICE_STARTED.store(false, Ordering::Release);
    }
}

/// Posts what a collection left: `ObjectFree` of the tags whose objects died
/// (every env's tags brought up to date, `heap::sweep_all_tags`) and the
/// `ClassUnload` extension event of each class unloaded, with its internal
/// name. A plain OS thread, as HotSpot's service thread is no Java thread to
/// the agent either: the callbacks get no `JNIEnv` they could use
/// (`ClassUnload`'s is the table's, on a thread that is not attached).
fn service_thread() {
    loop {
        let work = {
            let mut guard = SERVICE.lock();
            while !guard.collected && guard.unloaded.is_empty() {
                SERVICE_WAKE.wait(&mut guard);
            }
            let collected = std::mem::take(&mut guard.collected);
            let unloaded = std::mem::take(&mut guard.unloaded);
            ServiceWork {
                collected,
                unloaded,
            }
        };
        if work.collected {
            for shared in armed_vms(NativeEvent::ObjectFree) {
                heap::sweep_all_tags(&shared);
            }
        }
        if !work.unloaded.is_empty() {
            for shared in armed_vms(NativeEvent::ClassUnload) {
                let names: Vec<&String> = work
                    .unloaded
                    .iter()
                    .filter(|(vm, _)| *vm == shared.vm_identity)
                    .map(|(_, name)| name)
                    .collect();
                for (env, cb) in ext_callbacks(&shared, extensions::EXT_EVENT_CLASS_UNLOAD, 0) {
                    type ClassUnloadCallback =
                        unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, *const std::ffi::c_char);
                    // SAFETY: the agent's extension callback for `ClassUnload`.
                    let cb: ClassUnloadCallback = unsafe { std::mem::transmute(cb) };
                    for name in &names {
                        let Ok(text) = std::ffi::CString::new(name.as_str()) else {
                            continue;
                        };
                        // SAFETY: a call of the agent's callback.
                        unsafe {
                            cb(
                                env as *mut JvmtiNativeEnv,
                                jni::get_jni_env(),
                                text.as_ptr(),
                            )
                        };
                    }
                }
            }
        }
    }
}

/// The classes a collection of `shared` unloaded (`memory::gc`'s report):
/// queued for the `ClassUnload` extension event, posted by the service
/// thread. Asked only while the VM's gate is up.
pub(crate) fn note_unloaded_classes(shared: &SharedVm, names: &[&str]) {
    if names.is_empty() {
        return;
    }
    let mut guard = SERVICE.lock();
    guard
        .unloaded
        .extend(names.iter().map(|n| (shared.vm_identity, (*n).to_string())));
    SERVICE_WAKE.notify_one();
}

// --- GarbageCollectionStart / Finish (81 / 82) --------------------------------

/// `void (JNICALL *GarbageCollectionStart|Finish)(jvmtiEnv*)`.
type GcCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv);

/// The collectors' start / finish hook: `GarbageCollectionStart` /
/// `Finish` to every env of every armed VM, in the pause; after a finish,
/// the service thread's sweep of the tags. One relaxed load when no env
/// listens.
pub(crate) fn post_gc(start: bool) {
    let armed = GLOBAL_ARMED.load(Ordering::Relaxed);
    if armed & (NativeEvent::Gc as u32 | NativeEvent::ObjectFree as u32) == 0 {
        return;
    }
    if armed & NativeEvent::Gc as u32 != 0 {
        let event = if start {
            EVENT_GC_START
        } else {
            EVENT_GC_FINISH
        };
        for shared in armed_vms(NativeEvent::Gc) {
            for (env, cb) in callbacks(&shared, event, 0) {
                // SAFETY: the agent's callback of that event.
                let cb: GcCallback = unsafe { std::mem::transmute(cb) };
                // SAFETY: a call of the agent's callback.
                unsafe { cb(env as *mut JvmtiNativeEnv) };
            }
        }
    }
    if !start && armed & NativeEvent::ObjectFree as u32 != 0 {
        SERVICE.lock().collected = true;
        SERVICE_WAKE.notify_one();
    }
}

// --- DataDumpRequest (71) --------------------------------------------------

/// `SIGQUIT` (the attach listener's latch): `DataDumpRequest` to every env
/// of every armed VM.
pub(crate) fn post_data_dump_request() {
    for shared in armed_vms(NativeEvent::DataDump) {
        for (env, cb) in callbacks(&shared, EVENT_DATA_DUMP_REQUEST, 0) {
            // SAFETY: the agent's `DataDumpRequest` callback, `(jvmtiEnv*)`.
            let cb: GcCallback = unsafe { std::mem::transmute(cb) };
            // SAFETY: a call of the agent's callback.
            unsafe { cb(env as *mut JvmtiNativeEnv) };
        }
    }
}

// --- FieldAccess / FieldModification (63 / 64) ---------------------------------

/// What the field bytecodes hand [`post_field_watch`].
pub(crate) struct FieldEvent {
    pub(crate) declaring: ClassId,
    /// The field's index: among its class's statics, or its slot.
    pub(crate) field_index: usize,
    pub(crate) is_static: bool,
    /// The field descriptor's first byte.
    pub(crate) desc: u8,
    pub(crate) method: u64,
    pub(crate) location: i64,
    /// The instance, `None` for a static field.
    pub(crate) object: Option<ObjectRef>,
    /// The value to be stored (a modification), `None` for an access.
    pub(crate) value: Option<Value>,
}

type FieldAccessCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, i64, JObject, JObject, u64);
type FieldModificationCallback = unsafe extern "C" fn(
    *mut JvmtiNativeEnv,
    JNIEnv,
    JObject,
    u64,
    i64,
    JObject,
    JObject,
    u64,
    u8,
    u64,
);

/// A watched field's access or modification on thread `tid`: the
/// `FieldAccess` / `FieldModification` of every env that has it enabled for
/// the thread, when a `SetField*Watch` names the field.
pub(crate) fn post_field_watch(shared: &SharedVm, tid: u64, event: &FieldEvent) {
    let field_id = (if event.is_static { STATIC_FIELD_ID } else { 0 })
        | (u64::from(event.declaring.as_u32()) << 32)
        | (event.field_index as u64 & 0xFFFF_FFFF); // Cast: the index fits 32 bits
    let modification = event.value.is_some();
    let watched = {
        let jvmti = shared.debug.jvmti_env.lock();
        if modification {
            jvmti.c_field_modification_watches.contains(&field_id)
        } else {
            jvmti.c_field_access_watches.contains(&field_id)
        }
    };
    if !watched {
        return;
    }
    let kind = if modification {
        EVENT_FIELD_MODIFICATION
    } else {
        EVENT_FIELD_ACCESS
    };
    let klass = jni::class_id_to_jclass(event.declaring);
    for (env, cb) in callbacks(shared, kind, tid) {
        in_event_context(shared, tid, |jni_env, thread| {
            let object = event.object.map_or(0, jni::new_local_handle);
            match &event.value {
                None => {
                    // SAFETY: the agent's `FieldAccess` callback.
                    let cb: FieldAccessCallback = unsafe { std::mem::transmute(cb) };
                    // SAFETY: a call of the agent's callback.
                    unsafe {
                        cb(
                            env as *mut JvmtiNativeEnv,
                            jni_env,
                            thread,
                            event.method,
                            event.location,
                            klass,
                            object,
                            field_id,
                        )
                    };
                }
                Some(value) => {
                    let (sig, bits) = jvalue_of_field(event.desc, value);
                    // SAFETY: the agent's `FieldModification` callback.
                    let cb: FieldModificationCallback = unsafe { std::mem::transmute(cb) };
                    // SAFETY: a call of the agent's callback.
                    unsafe {
                        cb(
                            env as *mut JvmtiNativeEnv,
                            jni_env,
                            thread,
                            event.method,
                            event.location,
                            klass,
                            object,
                            field_id,
                            sig,
                            bits,
                        )
                    };
                }
            }
        });
    }
}

/// A field modification's `signature_type` and `jvalue`; a reference made a
/// local reference (in the caller's event context).
fn jvalue_of_field(desc: u8, value: &Value) -> (u8, u64) {
    let sig = if desc == b'[' { b'L' } else { desc };
    let bits = match value {
        Value::Object(Some(obj)) => jni::new_local_handle(*obj),
        Value::Object(None) => 0,
        Value::Long(v) => *v as u64, // Cast: the long's bits
        Value::Float(v) => u64::from(v.to_bits()),
        Value::Double(v) => v.to_bits(),
        Value::Int(v) => match desc {
            b'Z' | b'B' => u64::from(*v as u8),
            b'C' | b'S' => u64::from(*v as u16),
            _ => u64::from(*v as u32),
        },
        _ => 0,
    };
    (sig, bits)
}

// --- The monitor events (73-76) -----------------------------------------------

/// A monitor event of [`post_monitor_event`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum MonitorEvent {
    /// `MonitorWait`, with the wait's timeout in milliseconds.
    Wait(i64),
    /// `MonitorWaited`, whether the wait timed out.
    Waited(bool),
    /// `MonitorContendedEnter`.
    ContendedEnter,
    /// `MonitorContendedEntered`.
    ContendedEntered,
}

type MonitorCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, JObject);
type MonitorWaitCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, JObject, i64);
type MonitorWaitedCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, JObject, u8);

/// Monitor event `event` about `obj` on thread `tid` (a running mutator):
/// every env that has it enabled for the thread. Answers where `obj` is
/// after the callbacks, which may have collected.
pub(crate) fn post_monitor_event(
    shared: &SharedVm,
    tid: u64,
    event: MonitorEvent,
    obj: ObjectRef,
) -> ObjectRef {
    // Interpreter round i5 wave 103 (lane L1): the JDWP monitor events, from
    // the same points (`debug::post_jdwp_monitor_event`), before the C
    // callbacks, which may collect. One load while no request is in force.
    if shared.debug.debugger_gates.jdwp_monitor_events_armed() {
        use crate::debug::events::EventKind;
        let (jdwp_kind, trailer): (EventKind, Vec<u8>) = match event {
            MonitorEvent::Wait(timeout) => (EventKind::MonitorWait, timeout.to_be_bytes().to_vec()),
            MonitorEvent::Waited(timed_out) => {
                (EventKind::MonitorWaited, vec![u8::from(timed_out)])
            }
            MonitorEvent::ContendedEnter => (EventKind::MonitorContendedEnter, Vec::new()),
            MonitorEvent::ContendedEntered => (EventKind::MonitorContendedEntered, Vec::new()),
        };
        crate::debug::post_jdwp_monitor_event(shared, tid, jdwp_kind, obj, &trailer);
    }
    let kind = match event {
        MonitorEvent::Wait(_) => EVENT_MONITOR_WAIT,
        MonitorEvent::Waited(_) => EVENT_MONITOR_WAITED,
        MonitorEvent::ContendedEnter => EVENT_MONITOR_CONTENDED_ENTER,
        MonitorEvent::ContendedEntered => EVENT_MONITOR_CONTENDED_ENTERED,
    };
    let list = callbacks(shared, kind, tid);
    if list.is_empty() {
        return obj;
    }
    with_object_callbacks(
        shared,
        tid,
        obj,
        &list,
        |env, cb, jni_env, thread, handle| {
            let env = env as *mut JvmtiNativeEnv;
            // SAFETY (each arm): the agent's callback of that event, called with
            // live arguments.
            unsafe {
                match event {
                    MonitorEvent::Wait(timeout) => {
                        std::mem::transmute::<usize, MonitorWaitCallback>(cb)(
                            env, jni_env, thread, handle, timeout,
                        )
                    }
                    MonitorEvent::Waited(timed_out) => {
                        std::mem::transmute::<usize, MonitorWaitedCallback>(cb)(
                            env,
                            jni_env,
                            thread,
                            handle,
                            u8::from(timed_out),
                        )
                    }
                    MonitorEvent::ContendedEnter | MonitorEvent::ContendedEntered => {
                        std::mem::transmute::<usize, MonitorCallback>(cb)(
                            env, jni_env, thread, handle,
                        )
                    }
                }
            }
        },
    )
}

// --- VMObjectAlloc (84), SampledObjectAlloc (86) ---------------------------------

type AllocCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, JObject, JObject, i64);

/// The object's class as a local reference, and its size, for the
/// allocation events (in the caller's event context).
fn alloc_facts(
    shared: &SharedVm,
    jni_env: JNIEnv,
    handle: JObject,
    obj: ObjectRef,
    thread: &mut JvmThread,
) -> (JObject, i64) {
    type GetObjectClass = unsafe extern "C" fn(JNIEnv, JObject) -> JObject;
    // SAFETY: slot 31 of the `JNIEnv*` is `GetObjectClass`.
    let klass = unsafe {
        std::mem::transmute::<usize, GetObjectClass>(*(*jni_env).add(31))(jni_env, handle)
    };
    let ctx = crate::vm::vm_exec::NativeContextImpl { shared, thread };
    (klass, crate::runtime::instrument::object_size_of(&ctx, obj))
}

/// An allocation event of `kind` about `obj` on the running thread `thread`.
fn post_alloc(shared: &SharedVm, thread: &mut JvmThread, kind: JInt, obj: ObjectRef) -> ObjectRef {
    let tid = thread.thread_id.0;
    let list = callbacks(shared, kind, tid);
    if list.is_empty() {
        return obj;
    }
    let thread_ptr: *mut JvmThread = thread;
    // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    // item 1c, review): the event context installed the REGISTRY's address of
    // this thread unless one was installed already, so `alloc_facts`' borrow
    // (from `thread_ptr`) and a GC-safe JVMTI function the callback calls
    // (from the installed pointer) were two provenances of one `JvmThread`.
    // Installing `thread_ptr` around the deliveries makes them one.
    let deliver = || {
        with_object_callbacks(
            shared,
            tid,
            obj,
            &list,
            |env, cb, jni_env, jthread, handle| {
                let Some(current) = jni::jobject_to_obj(handle) else {
                    return;
                };
                // SAFETY: the running thread's own `JvmThread`, the pointer
                // installed around these deliveries; the callback below reborrows
                // it only after this borrow ends.
                let (klass, size) = alloc_facts(shared, jni_env, handle, current, unsafe {
                    &mut *thread_ptr
                });
                // SAFETY: the agent's callback of that event.
                let cb: AllocCallback = unsafe { std::mem::transmute(cb) };
                // SAFETY: a call of the agent's callback.
                unsafe {
                    cb(
                        env as *mut JvmtiNativeEnv,
                        jni_env,
                        jthread,
                        handle,
                        klass,
                        size,
                    )
                };
            },
        )
    };
    // SAFETY: `thread_ptr` comes from this function's `&mut`, which is not
    // used again until the deliveries return.
    unsafe { crate::runtime::interpreter::post_on_own_thread_ptr(thread_ptr, deliver) }
}

/// `VMObjectAlloc` of `obj`, which the VM just allocated for the program on
/// `thread` (see the module doc for the sites). Answers where `obj` is
/// after the callbacks.
pub(crate) fn post_vm_object_alloc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> ObjectRef {
    post_alloc(shared, thread, EVENT_VM_OBJECT_ALLOC, obj)
}

/// An allocation funnel allocated `obj` on `thread`: count its bytes toward
/// the thread's next sample, and post `SampledObjectAlloc` when the sample
/// falls due (every allocation for an interval of 0; else after a
/// geometrically distributed number of bytes of mean the interval, as
/// HotSpot draws them). Answers where `obj` is after the callbacks.
pub(crate) fn sample_allocation(
    shared: &SharedVm,
    thread: &mut JvmThread,
    obj: ObjectRef,
) -> ObjectRef {
    let size = {
        let ctx = crate::vm::vm_exec::NativeContextImpl {
            shared,
            thread: &mut *thread,
        };
        crate::runtime::instrument::object_size_of(&ctx, obj)
    };
    let tid = thread.thread_id.0;
    let due = {
        let mut jvmti = shared.debug.jvmti_env.lock();
        let interval = i64::from(jvmti.heap_sampling_interval);
        if interval == 0 {
            true
        } else {
            let remaining = jvmti
                .sampling_remaining
                .entry(tid)
                .or_insert_with(|| next_sample(interval));
            *remaining -= size;
            if *remaining <= 0 {
                *remaining = next_sample(interval);
                true
            } else {
                false
            }
        }
    };
    if !due {
        return obj;
    }
    post_alloc(shared, thread, EVENT_SAMPLED_OBJECT_ALLOC, obj)
}

/// The bytes until the next sample: geometric of mean `interval`.
fn next_sample(interval: i64) -> i64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(interval as u64); // Cast: the interval's bits
    let u = ((h.finish() >> 11) as f64 + 1.0) / ((1u64 << 53) as f64 + 1.0); // Cast: a 53-bit fraction
    ((-u.ln()) * interval as f64).max(1.0) as i64 // Cast: a byte count
}

/// `SetHeapSamplingInterval` changed `shared`'s interval: every thread's
/// next sample is drawn afresh.
pub(super) fn reset_sampling(shared: &SharedVm) {
    shared.debug.jvmti_env.lock().sampling_remaining.clear();
}

// --- NativeMethodBind (67) -----------------------------------------------------

type NativeBindCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject, u64, usize, *mut usize);

/// A JNI native `name` + `descriptor` of class `class_id` is being bound to
/// `address` on thread `tid`: `NativeMethodBind` to every env that has it
/// enabled, each handed the previous one's address. Answers the address to
/// bind (MEASURED: a callback's replacement is the one called).
pub(crate) fn post_native_method_bind(
    shared: &SharedVm,
    tid: u64,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
    address: usize,
) -> usize {
    let list = callbacks(shared, EVENT_NATIVE_METHOD_BIND, tid);
    if list.is_empty() {
        return address;
    }
    let Some(method) =
        rt::resolve_method_id_for_vm(shared.vm_identity, class_id.as_u32(), name, descriptor)
    else {
        return address;
    };
    let mut bound = address;
    for (env, cb) in list {
        in_event_context(shared, tid, |jni_env, thread| {
            let mut replacement = bound;
            // SAFETY: the agent's `NativeMethodBind` callback.
            let cb: NativeBindCallback = unsafe { std::mem::transmute(cb) };
            // SAFETY: a call of the agent's callback.
            unsafe {
                cb(
                    env as *mut JvmtiNativeEnv,
                    jni_env,
                    thread,
                    method,
                    bound,
                    &mut replacement,
                )
            };
            if replacement != 0 {
                bound = replacement;
            }
        });
    }
    bound
}

/// Where the funnel's `NativeMethodBind` of the registered (Rust) natives
/// stands for a VM (interpreter round i3 wave 83, lane L1;
/// `docs/feature-designs/jdk-only-mode.md`).
///
/// HotSpot posts the event when it links a native: at `RegisterNatives` (the
/// JDK's `registerNatives` in a class initializer) or at its first call (a
/// library symbol), once. This VM's registered natives have no link step, so
/// the funnel posts at a native's first run instead; it can tell a first run
/// only while it has watched every run since the VM started, i.e. when the
/// event was enabled in `Agent_OnLoad` and stayed enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RustBindTracking {
    /// No env has enabled `NativeMethodBind` yet.
    NotArmed,
    /// Enabled since the startup envs were bound: the funnel posts each
    /// registered native's first run.
    Tracking,
    /// Enabled in the live phase (natives already ran unwatched: HotSpot
    /// posts no bind for a native it linked before the enable, MEASURED by
    /// `L1W63JvmtiEvents`), or disabled after tracking: the funnel posts
    /// nothing for registered natives from then on.
    Lost,
}

/// The next [`RustBindTracking`] of a VM whose envs' `NativeMethodBind`
/// enable is now `enabled`, `startup` while `bind_startup_envs` attaches the
/// startup envs, and whether the funnel's hook is armed.
pub(crate) fn rust_bind_tracking_step(
    state: RustBindTracking,
    enabled: bool,
    startup: bool,
) -> (RustBindTracking, bool) {
    use RustBindTracking::{Lost, NotArmed, Tracking};
    match (state, enabled) {
        (NotArmed, false) => (NotArmed, false),
        (NotArmed, true) if startup => (Tracking, true),
        (NotArmed, true) => (Lost, false),
        (Tracking, true) => (Tracking, true),
        (Tracking, false) | (Lost, _) => (Lost, false),
    }
}

/// The native-call funnel runs the registered native `callback` with the
/// `NativeMethodBind` hook armed (`DebuggerGates::native_bind_hook_armed`):
/// the first time, `NativeMethodBind` of each `native` method it serves, with
/// `callback`'s address. Answers whether an event was posted (an agent's
/// callback ran, and may have collected).
///
/// The methods: every triple registered with `callback` that names a
/// `native` method of a loaded class (a callback standing in for Java
/// bytecode posts nothing: HotSpot runs that bytecode), or, for an
/// interpreter intrinsic's trampoline (`Object.hashCode`,
/// `System.arraycopy`: in no registration), the native the caller's invoke
/// names.
///
/// A replacement address the agent hands back is not bound: `callback` is a
/// Rust function, not a JNI one, so neither can stand for the other (an
/// agent's wrapper calling the address it was given would call a Rust
/// function with C arguments). The page records this gap.
#[cold]
#[inline(never)]
pub(crate) fn post_rust_native_method_bind(
    shared: &SharedVm,
    thread: &mut JvmThread,
    callback: usize,
) -> bool {
    if !shared
        .debug
        .jvmti_env
        .lock()
        .rust_native_binds_posted
        .insert(callback)
    {
        return false;
    }
    let methods: Vec<(ClassId, String, String)> = {
        let triples = shared.natives.native_methods.triples_running(callback);
        if triples.is_empty() {
            crate::runtime::interpreter::native_named_at_top_invoke(shared, thread, callback)
                .into_iter()
                .collect()
        } else {
            let cm = shared.classes.class_manager.read_recursive();
            let mut methods: Vec<(ClassId, String, String)> = Vec::new();
            for (class_name, name, descriptor) in triples {
                let Some(class_id) = cm.find_class_by_name_for_loader(
                    class_name,
                    crate::classloading::ClassLoaderId::Application,
                ) else {
                    continue;
                };
                let native = cm
                    .get_class(class_id)
                    .and_then(|class| class.find_method(name, descriptor))
                    .is_some_and(|method| method.is_native());
                if native
                    && !methods
                        .iter()
                        .any(|(c, n, d)| *c == class_id && n == name && d == descriptor)
                {
                    methods.push((class_id, name.to_owned(), descriptor.to_owned()));
                }
            }
            methods
        }
    };
    // Each method once, whichever of its callbacks ran first.
    let methods: Vec<(ClassId, String, String)> = {
        let mut jvmti = shared.debug.jvmti_env.lock();
        methods
            .into_iter()
            .filter(|method| jvmti.rust_native_bound_methods.insert(method.clone()))
            .collect()
    };
    let tid = thread.thread_id.0;
    // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
    // item 1c): the funnel holds this thread's `&mut`; its agents' callbacks
    // get that handle, not the registry's address of the same thread.
    crate::runtime::interpreter::post_on_own_thread(thread, || {
        for (class_id, name, descriptor) in &methods {
            let bound = post_native_method_bind(shared, tid, *class_id, name, descriptor, callback);
            if bound != callback {
                tracing::debug!(
                    "NativeMethodBind: the replacement address of a registered native is not bound ({name}{descriptor})"
                );
            }
        }
    });
    !methods.is_empty()
}

// --- ResourceExhausted (80) ----------------------------------------------------

/// `JVMTI_RESOURCE_EXHAUSTED_OOM_ERROR`, `_JAVA_HEAP`, `_THREADS`.
const EXHAUSTED_OOM_ERROR: JInt = 1;
const EXHAUSTED_JAVA_HEAP: JInt = 2;
const EXHAUSTED_THREADS: JInt = 4;

type ExhaustedCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JInt, *const u8, *const std::ffi::c_char);

/// The VM is about to raise an `OutOfMemoryError` with `message` on thread
/// `tid`: HotSpot's `[<uptime>][error][jvmti] Posting Resource Exhausted
/// event: <message>` line on standard output, then `ResourceExhausted` to
/// every env that has it enabled. The flags as HotSpot's (MEASURED: the
/// array limit is `OOM_ERROR` alone): `JAVA_HEAP` for an exhausted heap,
/// `THREADS` for a thread the OS would not start.
pub(crate) fn post_resource_exhausted(shared: &SharedVm, tid: u64, message: &str) {
    let list = callbacks(shared, EVENT_RESOURCE_EXHAUSTED, tid);
    if list.is_empty() {
        return;
    }
    {
        use std::io::Write as _;
        let uptime = shared
            .debug
            .diagnostic_counters
            .start_instant()
            .elapsed()
            .as_secs_f64();
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "[{uptime:.3}s][error][jvmti] Posting Resource Exhausted event: {message}"
        );
        let _ = out.flush();
    }
    let flags = if message == "Java heap space" || message == "GC overhead limit exceeded" {
        EXHAUSTED_OOM_ERROR | EXHAUSTED_JAVA_HEAP
    } else if message.starts_with("unable to create native thread") {
        EXHAUSTED_OOM_ERROR | EXHAUSTED_THREADS
    } else {
        EXHAUSTED_OOM_ERROR
    };
    let Ok(text) = std::ffi::CString::new(message) else {
        return;
    };
    for (env, cb) in list {
        in_event_context(shared, tid, |jni_env, _thread| {
            // SAFETY: the agent's `ResourceExhausted` callback.
            let cb: ExhaustedCallback = unsafe { std::mem::transmute(cb) };
            // SAFETY: a call of the agent's callback.
            unsafe {
                cb(
                    env as *mut JvmtiNativeEnv,
                    jni_env,
                    flags,
                    std::ptr::null(),
                    text.as_ptr(),
                )
            };
        });
    }
}

// --- Virtual threads (87 / 88, 47 / 48) ----------------------------------------

/// A virtual thread's lifecycle event of [`post_virtual_thread`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VirtualThreadEvent {
    Start,
    End,
    Mount,
    Unmount,
}

type VirtualThreadCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, JNIEnv, JObject);

/// Virtual thread `tid`, mounted and running on this carrier, `event`d: to
/// every env that listens, with the virtual thread as the `jthread`, in
/// its event context (MEASURED order: start, mount at the first mount;
/// unmount at each park; mount at each resume; unmount, end at the end).
pub(crate) fn post_virtual_thread(shared: &SharedVm, tid: u64, event: VirtualThreadEvent) {
    let list = match event {
        VirtualThreadEvent::Start => callbacks(shared, EVENT_VIRTUAL_THREAD_START, tid),
        VirtualThreadEvent::End => callbacks(shared, EVENT_VIRTUAL_THREAD_END, tid),
        VirtualThreadEvent::Mount => {
            ext_callbacks(shared, extensions::EXT_EVENT_VIRTUAL_THREAD_MOUNT, tid)
        }
        VirtualThreadEvent::Unmount => {
            ext_callbacks(shared, extensions::EXT_EVENT_VIRTUAL_THREAD_UNMOUNT, tid)
        }
    };
    for (env, cb) in list {
        in_event_context(shared, tid, |jni_env, thread| {
            // SAFETY: the agent's callback of that event.
            let cb: VirtualThreadCallback = unsafe { std::mem::transmute(cb) };
            // SAFETY: a call of the agent's callback.
            unsafe { cb(env as *mut JvmtiNativeEnv, jni_env, thread) };
        });
    }
}

// --- Compiled code (68-70) -------------------------------------------------------

type CompiledLoadCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, u64, JInt, usize, JInt, usize, usize);
type CompiledUnloadCallback = unsafe extern "C" fn(*mut JvmtiNativeEnv, u64, usize);
type DynamicCodeCallback =
    unsafe extern "C" fn(*mut JvmtiNativeEnv, *const std::ffi::c_char, usize, JInt);

/// Install or remove the JIT's code-event sink.
fn install_code_sink(on: bool) {
    cratonvm_jit::code_events::set_jvmti_sink(if on { Some(code_event_sink) } else { None });
}

/// The JIT published or retired a body: the compiled-code events to every
/// env of every armed VM.
fn code_event_sink(event: &cratonvm_jit::code_events::JvmtiCodeEvent) {
    for shared in armed_vms(NativeEvent::CompiledCode) {
        post_code_event(&shared, event, None);
    }
}

/// `event` to every env of `shared` that listens (`only`: to that env, the
/// `GenerateEvents` replay).
fn post_code_event(
    shared: &SharedVm,
    event: &cratonvm_jit::code_events::JvmtiCodeEvent,
    only: Option<usize>,
) {
    use cratonvm_jit::code_events::JvmtiCodeEvent as E;
    let kind = match event {
        E::MethodLoad { .. } => EVENT_COMPILED_METHOD_LOAD,
        E::MethodUnload { .. } => EVENT_COMPILED_METHOD_UNLOAD,
        E::Stub { .. } => EVENT_DYNAMIC_CODE_GENERATED,
    };
    let list: Vec<(usize, usize)> = callbacks(shared, kind, 0)
        .into_iter()
        .filter(|&(env, _)| only.is_none_or(|o| o == env))
        .collect();
    if list.is_empty() {
        return;
    }
    let method = |class: &str, name: &str, descriptor: &str| -> Option<u64> {
        let class_id = shared
            .classes
            .class_manager
            .read()
            .class_store
            .iter()
            .find(|c| &*c.name == class)
            .map(|c| c.id)?;
        rt::resolve_method_id_for_vm(shared.vm_identity, class_id.as_u32(), name, descriptor)
    };
    for (env, cb) in list {
        let env = env as *mut JvmtiNativeEnv;
        match event {
            E::MethodLoad {
                class,
                name,
                descriptor,
                start,
                len,
            } => {
                let Some(m) = method(class, name, descriptor) else {
                    continue;
                };
                let size = JInt::try_from(*len).unwrap_or(JInt::MAX);
                // SAFETY: the agent's `CompiledMethodLoad` callback.
                unsafe {
                    std::mem::transmute::<usize, CompiledLoadCallback>(cb)(
                        env, m, size, *start, 0, 0, 0,
                    )
                };
            }
            E::MethodUnload {
                class,
                name,
                descriptor,
                start,
            } => {
                let Some(m) = method(class, name, descriptor) else {
                    continue;
                };
                // SAFETY: the agent's `CompiledMethodUnload` callback.
                unsafe { std::mem::transmute::<usize, CompiledUnloadCallback>(cb)(env, m, *start) };
            }
            E::Stub { name, start, len } => {
                let Ok(text) = std::ffi::CString::new(name.as_str()) else {
                    continue;
                };
                let size = JInt::try_from(*len).unwrap_or(JInt::MAX);
                // SAFETY: the agent's `DynamicCodeGenerated` callback.
                unsafe {
                    std::mem::transmute::<usize, DynamicCodeCallback>(cb)(
                        env,
                        text.as_ptr(),
                        *start,
                        size,
                    )
                };
            }
        }
    }
}

// --- Field watches (slots 41-44) -------------------------------------------

/// `JVMTI_ERROR_NOT_FOUND`.
const ERR_NOT_FOUND: JInt = 41;

/// The `jfieldID` of a field of `klass`, checked as `GetFieldName` checks
/// one.
fn watched_field(env: *mut JvmtiNativeEnv, klass: JObject, field: u64) -> Result<u64, JInt> {
    with_field(env, klass, field, |_, _| field)
}

/// The four watch slots: the live phase, the capability (bit field `cap`),
/// the class and the field, then the change to the VM's watch set (`set`:
/// add, refused with `DUPLICATE` when present; else remove, refused with
/// `NOT_FOUND` when absent; MEASURED), and the gates republished.
fn change_watch(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
    cap: usize,
    modification: bool,
    set: bool,
) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    match live_env(env) {
        Ok(e) if !has_cap(&e.state.lock().capabilities, cap) => return ERR_MUST_POSSESS_CAPABILITY,
        Ok(_) => {}
        Err(code) => return code,
    }
    let field = match watched_field(env, klass, field) {
        Ok(field) => field,
        Err(code) => return code,
    };
    {
        let mut jvmti = shared.debug.jvmti_env.lock();
        let watches = if modification {
            &mut jvmti.c_field_modification_watches
        } else {
            &mut jvmti.c_field_access_watches
        };
        if set && !watches.insert(field) {
            return ERR_DUPLICATE;
        }
        if !set && !watches.remove(&field) {
            return ERR_NOT_FOUND;
        }
    }
    refresh_event_gates(&shared);
    ERR_NONE
}

/// Slot 41: `SetFieldAccessWatch(env, klass, field)`
/// (`can_generate_field_access_events`).
pub(super) extern "C" fn set_field_access_watch(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
) -> JInt {
    change_watch(env, klass, field, CAP_FIELD_ACCESS_EVENTS, false, true)
}

/// Slot 42: `ClearFieldAccessWatch(env, klass, field)`.
pub(super) extern "C" fn clear_field_access_watch(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
) -> JInt {
    change_watch(env, klass, field, CAP_FIELD_ACCESS_EVENTS, false, false)
}

/// Slot 43: `SetFieldModificationWatch(env, klass, field)`
/// (`can_generate_field_modification_events`).
pub(super) extern "C" fn set_field_modification_watch(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
) -> JInt {
    change_watch(env, klass, field, CAP_FIELD_MODIFICATION_EVENTS, true, true)
}

/// Slot 44: `ClearFieldModificationWatch(env, klass, field)`.
pub(super) extern "C" fn clear_field_modification_watch(
    env: *mut JvmtiNativeEnv,
    klass: JObject,
    field: u64,
) -> JInt {
    change_watch(
        env,
        klass,
        field,
        CAP_FIELD_MODIFICATION_EVENTS,
        true,
        false,
    )
}

// --- GenerateEvents (slot 123) ---------------------------------------------

/// Slot 123: `GenerateEvents(env, event_type)` (live phase): the
/// `CompiledMethodLoad` events of the bodies compiled so far
/// (`can_generate_compiled_method_load_events`) or the
/// `DynamicCodeGenerated` events of the stubs generated so far, to this env
/// (MEASURED: both post); any other event `ILLEGAL_ARGUMENT` (MEASURED: 0 and
/// `VMInit` are 103).
pub(super) extern "C" fn generate_events(env: *mut JvmtiNativeEnv, event_type: JInt) -> JInt {
    let shared = match live_phase_vm(env) {
        Ok(shared) => shared,
        Err(code) => return code,
    };
    let e = match live_env(env) {
        Ok(e) => e,
        Err(code) => return code,
    };
    let stubs = match event_type {
        EVENT_COMPILED_METHOD_LOAD => {
            if !has_cap(
                &e.state.lock().capabilities,
                CAP_COMPILED_METHOD_LOAD_EVENTS,
            ) {
                return ERR_MUST_POSSESS_CAPABILITY;
            }
            false
        }
        EVENT_DYNAMIC_CODE_GENERATED => true,
        _ => return ERR_ILLEGAL_ARGUMENT,
    };
    for event in cratonvm_jit::code_events::live_code(stubs) {
        post_code_event(&shared, &event, Some(e.addr()));
    }
    ERR_NONE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each wave-63 event wants its capability, and only the events HotSpot
    /// refuses per thread are global.
    #[test]
    fn the_wave_63_events_want_their_capabilities() {
        for event in [
            EVENT_FIELD_ACCESS,
            EVENT_NATIVE_METHOD_BIND,
            EVENT_MONITOR_WAIT,
            EVENT_GC_START,
            EVENT_OBJECT_FREE,
            EVENT_VM_OBJECT_ALLOC,
            EVENT_SAMPLED_OBJECT_ALLOC,
            EVENT_VIRTUAL_THREAD_START,
        ] {
            assert!(
                event_capability(event).is_some_and(|c| c.iter().any(|&w| w != 0)),
                "{event}"
            );
        }
        assert!(is_global_event(EVENT_DATA_DUMP_REQUEST));
        assert!(!is_global_event(EVENT_MONITOR_WAIT));
    }

    /// Wave 83: the funnel posts the registered natives' binds only when the
    /// event was enabled while the startup envs were bound, and stops for
    /// good once it was disabled or enabled late (natives ran unwatched).
    #[test]
    fn the_rust_bind_tracking_starts_only_at_startup() {
        use RustBindTracking::{Lost, NotArmed, Tracking};
        assert_eq!(
            rust_bind_tracking_step(NotArmed, false, true),
            (NotArmed, false)
        );
        assert_eq!(
            rust_bind_tracking_step(NotArmed, false, false),
            (NotArmed, false)
        );
        assert_eq!(
            rust_bind_tracking_step(NotArmed, true, true),
            (Tracking, true)
        );
        assert_eq!(
            rust_bind_tracking_step(NotArmed, true, false),
            (Lost, false)
        );
        assert_eq!(
            rust_bind_tracking_step(Tracking, true, false),
            (Tracking, true)
        );
        assert_eq!(
            rust_bind_tracking_step(Tracking, false, false),
            (Lost, false)
        );
        assert_eq!(rust_bind_tracking_step(Lost, true, true), (Lost, false));
        assert_eq!(rust_bind_tracking_step(Lost, true, false), (Lost, false));
    }

    /// The sampling distance is positive and averages near the interval.
    #[test]
    fn the_sampling_distance_averages_the_interval() {
        let interval = 512 * 1024;
        let n = 4000;
        let sum: i64 = (0..n).map(|_| next_sample(interval)).sum();
        assert!((0..100).all(|_| next_sample(interval) >= 1));
        let mean = sum / n;
        assert!(mean > interval / 2 && mean < interval * 2, "mean {mean}");
    }
}
