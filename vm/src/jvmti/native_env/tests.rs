use super::*;
use crate::runtime::frame::Frame;
use crate::types::Value;
use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// `set` less the capabilities of `less`.
fn without(set: Caps, less: Caps) -> Caps {
    let mut out = set;
    for (word, drop) in out.iter_mut().zip(less) {
        *word &= !drop;
    }
    out
}

static HITS: AtomicUsize = AtomicUsize::new(0);
static HITS_AT_9: AtomicUsize = AtomicUsize::new(0);
static HITS_WITH_JNI_ENV: AtomicUsize = AtomicUsize::new(0);

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_breakpoint(
    _env: *mut JvmtiNativeEnv,
    jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    location: i64,
) {
    HITS.fetch_add(1, Ordering::SeqCst);
    if location == 9 {
        HITS_AT_9.fetch_add(1, Ordering::SeqCst);
    }
    if !jni_env.is_null() {
        HITS_WITH_JNI_ENV.fetch_add(1, Ordering::SeqCst);
    }
}

type GetEnvFn = extern "C" fn(jni::JavaVM, *mut *mut c_void, JInt) -> JInt;
type Caps = [u32; CAPABILITY_WORDS];

/// Function `n` (1-based, as `jvmti.h` numbers them) of a `jvmtiEnv*`.
fn function(env: *mut c_void, n: usize) -> usize {
    // SAFETY: a `jvmtiEnv*` points at its function-table pointer, and
    // the table has `JVMTI_FUNCTION_COUNT` entries.
    unsafe { *(*(env as *const *const usize)).add(n - 1) }
}

/// `GetEnv` through the invocation table (slot 6).
fn get_env(version: JInt) -> (JInt, *mut c_void) {
    let java_vm = jni::get_java_vm();
    // SAFETY: the invocation table's slot 6 is `GetEnv`.
    let get_env: GetEnvFn =
        unsafe { std::mem::transmute::<usize, GetEnvFn>(*(*java_vm).add(6)) };
    let mut env: *mut c_void = std::ptr::null_mut();
    let code = get_env(java_vm, &mut env, version);
    (code, env)
}

/// A VM fixture with the weak self-reference `Vm::new()` installs (the
/// delivery path reaches `SharedVm::get_arc`) and its JVMTI manager.
fn vm_fixture() -> Arc<SharedVm> {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    *shared.self_arc.write() = Some(Arc::downgrade(&shared));
    let vm = shared.vm_identity;
    rt::install_manager_for_vm(vm, Arc::new(rt::JvmtiEventManager::new_for_vm(vm)));
    // As if a startup agent had acquired the OnLoad-only capabilities
    // (wave 46): every test's live-phase env may add [`POTENTIAL`].
    shared.debug.jvmti_env.lock().c_table.onload_acquired = ONLOAD_ONLY;
    shared
}

/// Wave 46: in the live phase the three capabilities of the local and
/// monitor functions are potential only as far as a startup env acquired
/// them ([`ONLOAD_ONLY`], HotSpot 25.0.3's answer), each on its own.
#[test]
fn the_onload_only_capabilities_follow_what_a_startup_env_acquired() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    shared.debug.jvmti_env.lock().c_table.onload_acquired = [0; CAPABILITY_WORDS];
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let potential: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 140)) };
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let locals: Caps = [CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0];
    let mut caps: Caps = [0; CAPABILITY_WORDS];
    assert_eq!(potential(env, &mut caps), ERR_NONE);
    for (have, only) in caps.iter().zip(ONLOAD_ONLY) {
        assert_eq!(have & only, 0, "none acquired: none potential");
    }
    assert_eq!(add(env, &locals), ERR_NOT_AVAILABLE);
    shared.debug.jvmti_env.lock().c_table.onload_acquired = locals;
    assert_eq!(potential(env, &mut caps), ERR_NONE);
    assert_eq!(
        caps[0] & CAN_ACCESS_LOCAL_VARIABLES,
        CAN_ACCESS_LOCAL_VARIABLES
    );
    assert_eq!(caps[0] & CAN_GET_CURRENT_CONTENDED_MONITOR, 0);
    assert_eq!(caps[1] & CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO, 0);
    assert_eq!(add(env, &locals), ERR_NONE);
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(shared.vm_identity);
    jni::clear_jni_context();
}

/// Interpreter round i6 wave 132, lane L1 (i126-L1;
/// `CRATONVM_JVMTI_C_AGENT_KEEPS_LOCALS_ALIVE`): a startup agent that
/// acquired `can_access_local_variables` in `Agent_OnLoad` makes every
/// local of the VM it is bound to a root (`debugger_observes_locals`),
/// as a JDWP port does; one that did not leaves the VM's answer false;
/// a live-phase env's add (offered because a startup env acquired it,
/// `vm_fixture`) does not set it.
#[test]
fn a_startup_agent_with_local_access_makes_every_local_a_root() {
    let _lock = rt::jvmti_registry_test_lock();
    let observes =
        |shared: &Arc<SharedVm>| crate::runtime::jvmti::debugger_observes_locals(shared);
    for wants_locals in [false, true] {
        let shared = vm_fixture();
        let vm = shared.vm_identity;
        jni::set_jni_context_arc(Arc::clone(&shared));
        let mut slot: Option<*mut c_void> = None;
        let envs = collect_startup_envs(|| {
            let (code, env) = get_env(0x3001_0200);
            assert_eq!(code, jni::JNI_OK);
            // SAFETY: the slot's `jvmti.h` signature.
            let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
                unsafe { std::mem::transmute(function(env, 142)) };
            let caps: Caps = if wants_locals {
                [CAN_ACCESS_LOCAL_VARIABLES | CAN_SUSPEND, 0, 0, 0]
            } else {
                [CAN_SUSPEND, 0, 0, 0]
            };
            assert_eq!(add(env, &caps), ERR_NONE);
            slot = Some(env);
        });
        let env = slot.expect("the agent obtained an env");
        if crate::config::JDWP_SERVER_COMPILED_IN {
            assert!(shared.config.jdwp_port.is_none(), "no JDWP port");
        }
        assert!(!observes(&shared), "not before the bind");
        shared.debug.jvmti_env.lock().c_table.startup_native_envs = envs;
        bind_startup_envs(&shared);
        let expected = wants_locals && shared.debug.debugger_gates.c_agent_keeps_locals_alive();
        assert_eq!(observes(&shared), expected, "wants_locals={wants_locals}");
        // A live-phase env adding it changes nothing.
        if !wants_locals {
            let (code, live) = get_env(0x3001_0200);
            assert_eq!(code, jni::JNI_OK);
            // SAFETY (both transmutes): the slot's `jvmti.h` signature.
            let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
                unsafe { std::mem::transmute(function(live, 142)) };
            let dispose: extern "C" fn(*mut c_void) -> JInt =
                unsafe { std::mem::transmute(function(live, 127)) };
            shared.raise_vm_init_level(3);
            let _ = add(live, &[CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0]);
            assert!(!observes(&shared), "a live-phase add is not a start fact");
            assert_eq!(dispose(live), ERR_NONE);
        }
        // SAFETY: the slot's `jvmti.h` signature.
        let dispose: extern "C" fn(*mut c_void) -> JInt =
            unsafe { std::mem::transmute(function(env, 127)) };
        assert_eq!(dispose(env), ERR_NONE);
        assert_eq!(observes(&shared), expected, "never undone");
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
    }
}

/// JIT round 21 wave 24 (lane flipprep, flip review item 2): the per-VM
/// answer the fresh-allocation volatile-store fence elision declines on
/// follows what this VM's startup envs acquired of
/// `can_access_local_variables` only (another VM's record, or another
/// OnLoad capability, does not count), and is `true` while the VM's
/// JVMTI lock is held (fail-closed).
#[test]
fn flipprep_locals_capability_may_be_held_is_per_vm_and_fail_closed() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let other = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    assert!(!locals_capability_may_be_held(&shared), "no agent: no capability");
    shared.debug.jvmti_env.lock().c_table.onload_acquired =
        [CAN_GET_OWNED_MONITOR_INFO, 0, 0, 0];
    assert!(!locals_capability_may_be_held(&shared), "another OnLoad capability");
    shared.debug.jvmti_env.lock().c_table.onload_acquired =
        [CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0];
    assert!(locals_capability_may_be_held(&shared));
    assert!(!locals_capability_may_be_held(&other), "per VM");
    let held = other.debug.jvmti_env.lock();
    assert!(locals_capability_may_be_held(&other), "lock held: fail-closed");
    drop(held);
    assert!(!locals_capability_may_be_held(&other));
}

/// t1 wave 5, lane M: a live-phase `can_tag_objects` raises the per-VM
/// gate and logs the in-flight refusal once (the epoch advances); a
/// second acquisition logs nothing; another VM is untouched.
#[test]
fn t1m_live_heap_walker_refuses_in_flight_compiles_once() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let other = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    assert!(!shared.debug.debugger_gates.c_agent_walks_heap());
    let before = cratonvm_jit::jit_invalidation_epoch();
    assert!(note_live_heap_walker(&shared));
    assert!(shared.debug.debugger_gates.c_agent_walks_heap());
    assert!(cratonvm_jit::jit_invalidation_epoch() > before, "a record was logged");
    assert!(!note_live_heap_walker(&shared), "logged on the first gain only");
    assert!(!other.debug.debugger_gates.c_agent_walks_heap(), "per VM");
}

/// t1 wave 2, lane M: exactly the three capabilities under which HotSpot's
/// C2 annotates safepoints for the escape barrier mark the VM as having a
/// monitor observer; another OnLoad capability does not.
#[test]
fn t1m_onload_acquired_observes_monitors_names_the_three_capabilities() {
    assert!(!onload_acquired_observes_monitors(&[0; CAPABILITY_WORDS]));
    assert!(!onload_acquired_observes_monitors(&[
        CAN_GET_CURRENT_CONTENDED_MONITOR | CAN_POP_FRAME,
        0,
        0,
        0
    ]));
    assert!(onload_acquired_observes_monitors(&[CAN_GET_OWNED_MONITOR_INFO, 0, 0, 0]));
    assert!(onload_acquired_observes_monitors(&[CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0]));
    assert!(onload_acquired_observes_monitors(&[
        0,
        CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO,
        0,
        0
    ]));
    // The stack-depth bit read from the wrong word is not it.
    assert!(!onload_acquired_observes_monitors(&[
        CAN_GET_OWNED_MONITOR_STACK_DEPTH_INFO & !(CAN_GET_OWNED_MONITOR_INFO
            | CAN_ACCESS_LOCAL_VARIABLES),
        0,
        0,
        0
    ]));
}

/// Interpreter round i2 wave 57, lane L1: slot 72,
/// `GetLocalVariableTable`, in HotSpot's order (MEASURED,
/// `L1W57JvmtiLocalVariableTable`): the capability (99), a NULL method
/// (23), a NULL out-parameter (100), then a native method (104) and a
/// method without a table (101); and a table's rows.
#[test]
fn the_local_variable_table_answers_in_hotspots_order() {
    use cratonvm_reader::attribute::LocalVariableEntry;
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    jni::set_jni_context_arc(Arc::clone(&shared));
    let name = "cratonvm/test/W57Locals";
    let cid = loop_class(&shared, name);
    if let Some(class) = shared.classes.class_manager.write().get_class_mut(cid) {
        let mut with_table = class.methods[0].clone();
        with_table.name = Arc::from("withTable");
        if let Some(LazyAttribute::Decoded(Attribute::Code(code))) =
            with_table.attributes.first_mut()
        {
            code.attributes
                .push(Attribute::LocalVariableTable(vec![LocalVariableEntry {
                    start_pc: 2,
                    length: 20,
                    name_index: 0,
                    descriptor_index: 0,
                    index: 1,
                }]));
        }
        let mut native = class.methods[0].clone();
        native.name = Arc::from("nativeOne");
        native.access_flags = MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE;
        native.attributes.clear();
        class.methods.push(with_table);
        class.methods.push(native);
    }
    let jmid = |index: u64| (u64::from(cid.as_u32()) << 32) | index;
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let table: extern "C" fn(*mut c_void, u64, *mut JInt, *mut *mut LocalVariableRow) -> JInt =
        unsafe { std::mem::transmute(function(env, 72)) };
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let mut count: JInt = -1;
    let mut rows: *mut LocalVariableRow = std::ptr::null_mut();
    assert_eq!(
        table(env, jmid(1), &mut count, &mut rows),
        ERR_MUST_POSSESS_CAPABILITY
    );
    assert_eq!(add(env, &[CAN_ACCESS_LOCAL_VARIABLES, 0, 0, 0]), ERR_NONE);
    assert_eq!(table(env, 0, &mut count, &mut rows), ERR_INVALID_METHODID);
    assert_eq!(
        table(env, jmid(1), std::ptr::null_mut(), &mut rows),
        ERR_NULL_POINTER
    );
    assert_eq!(
        table(env, jmid(2), &mut count, &mut rows),
        104,
        "a native method"
    );
    assert_eq!(
        table(env, jmid(0), &mut count, &mut rows),
        ERR_ABSENT_INFORMATION
    );
    assert_eq!(count, -1, "nothing written");
    assert_eq!(table(env, jmid(1), &mut count, &mut rows), ERR_NONE);
    assert_eq!(count, 1);
    // SAFETY: one row, just answered.
    let row = unsafe { *rows };
    assert_eq!((row.start_location, row.length, row.slot), (2, 20, 1));
    assert!(
        row.generic_signature.is_null(),
        "no LocalVariableTypeTable row"
    );
    for block in [row.name, row.signature, rows.cast::<u8>()] {
        assert_eq!(dealloc(env, block), ERR_NONE);
    }
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(shared.vm_identity);
    jni::clear_jni_context();
}

/// Wave 51: slot 10, `GetOwnedMonitorInfo`, is in the table behind
/// `can_get_owned_monitor_info`, which is potential (as one of the
/// OnLoad-only capabilities) and refused to an env without it.
#[test]
fn get_owned_monitor_info_needs_its_capability() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    assert_ne!(
        function(env, 10),
        not_available as *const () as usize,
        "slot 10 is implemented"
    );
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let potential: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 140)) };
    let owned: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut *mut JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 10)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let mut caps: Caps = [0; CAPABILITY_WORDS];
    assert_eq!(potential(env, &mut caps), ERR_NONE);
    assert_eq!(
        caps[0] & CAN_GET_OWNED_MONITOR_INFO,
        CAN_GET_OWNED_MONITOR_INFO
    );
    assert_eq!(
        ONLOAD_ONLY[0] & CAN_GET_OWNED_MONITOR_INFO,
        CAN_GET_OWNED_MONITOR_INFO
    );
    let mut count: JInt = -1;
    let mut monitors: *mut JObject = std::ptr::null_mut();
    assert_eq!(
        owned(env, 0, &mut count, &mut monitors),
        ERR_MUST_POSSESS_CAPABILITY
    );
    assert_eq!(count, -1, "nothing written");
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(shared.vm_identity);
    jni::clear_jni_context();
}

/// javac's `for (i = 0; i < 10; i++) s += i; return s;`: the
/// `if_icmpge` at pc 9 runs eleven times.
fn loop_code() -> Vec<u8> {
    vec![
        0x10, 10, 0x3b, // 0: bipush 10; istore_0
        0x03, 0x3c, // 3: iconst_0; istore_1
        0x03, 0x3d, // 5: iconst_0; istore_2
        0x1c, 0x1a, 0xa2, 0x00, 13, // 7: iload_2; iload_0; if_icmpge +13 (-> 22)
        0x1b, 0x1c, 0x60, 0x3c, // 12: iload_1; iload_2; iadd; istore_1
        0x84, 2, 1, // 16: iinc 2 1
        0xa7, 0xff, 0xf4, // 19: goto -12 (-> 7)
        0x1b, 0xac, // 22: iload_1; ireturn
    ]
}

/// A class `name` declaring the static loop method `jvmtiBpLoop()I`;
/// its `jmethodID` is `class id << 32`.
fn loop_class(shared: &SharedVm, name: &str) -> ClassId {
    let cid = shared
        .classes
        .class_manager
        .write()
        .try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates");
    if let Some(class) = shared.classes.class_manager.write().get_class_mut(cid) {
        class.methods = vec![cratonvm_reader::method::ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC,
            name: Arc::from("jvmtiBpLoop"),
            descriptor: Arc::from("()I"),
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 2,
                    max_locals: 3,
                    code: cratonvm_reader::ByteView::from_vec(loop_code()),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }];
    }
    cid
}

/// Run the loop method of `cid` on a thread with id `tid`.
fn run_loop(shared: &Arc<SharedVm>, cid: ClassId, name: &str, tid: u64) {
    let mut thread = JvmThread::new(ThreadId(tid), "w13-native-agent");
    let frame = Frame::new(
        cid,
        name.to_string(),
        "jvmtiBpLoop".to_string(),
        "()I".to_string(),
        None,
        loop_code(),
        Vec::new(),
        2,
        3,
        &[],
    );
    let r = crate::runtime::interpreter::execute_prebuilt_frame(shared, &mut thread, frame);
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
}

/// Members of `jvmtiEventCallbacks` as `jvmti.h` orders them.
fn callbacks_with(members: &[(JInt, usize)]) -> [usize; CALLBACK_SLOTS] {
    let mut callbacks = [0usize; CALLBACK_SLOTS];
    for &(event, function) in members {
        callbacks[usize::try_from(event - MIN_EVENT).expect("an event")] = function;
    }
    callbacks
}

/// A native agent's life through the C table: `GetEnv` for JVMTI 1.2,
/// capability negotiation (only the delivered events' capabilities are
/// granted), `SetEventCallbacks` + `SetEventNotificationMode`,
/// `SetBreakpoint` on the `if_icmpge` of a ten-trip loop — eleven C
/// callbacks through `jvmti.h`'s `Breakpoint` member (event 62, member
/// 12) — `GetMethodName` / `GetErrorName` / `Deallocate`,
/// `ClearBreakpoint`, `DisposeEnvironment`; an unimplemented slot answers
/// `JVMTI_ERROR_NOT_AVAILABLE`.
#[test]
fn a_native_agent_sets_a_breakpoint_and_receives_it_through_the_c_table() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    HITS.store(0, Ordering::SeqCst);
    HITS_AT_9.store(0, Ordering::SeqCst);
    HITS_WITH_JNI_ENV.store(0, Ordering::SeqCst);

    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    assert!(!env.is_null());
    assert_eq!(
        get_env(0x3063_0000).0,
        jni::JNI_EVERSION,
        "JVMTI 99 is not provided"
    );
    // Round i2 wave 53, measured on HotSpot 25: 9 and 11 take minor 0
    // only, 12 is not provided.
    for refused in [0x3009_0100, 0x300B_0100, 0x300C_0000, 0x300A_0000] {
        assert_eq!(get_env(refused).0, jni::JNI_EVERSION, "{refused:#x}");
    }

    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let version: extern "C" fn(*mut c_void, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 88)) };
    let mut v = 0;
    assert_eq!(version(env, &mut v), ERR_NONE);
    assert_eq!(v, JVMTI_VERSION_25);
    let potential: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 140)) };
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let get_caps: extern "C" fn(*mut c_void, *mut Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 89)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 38)) };
    let clear_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 39)) };
    let method_name: extern "C" fn(
        *mut c_void,
        u64,
        *mut *mut u8,
        *mut *mut u8,
        *mut *mut u8,
    ) -> JInt = unsafe { std::mem::transmute(function(env, 64)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let error_name: extern "C" fn(*mut c_void, JInt, *mut *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 128)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let get_tag: extern "C" fn(*mut c_void, JObject, *mut i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 106)) };
    let mut tag = 0i64;
    assert_eq!(
        get_tag(env, 0, &mut tag),
        ERR_MUST_POSSESS_CAPABILITY,
        "GetTag (wave 63) wants can_tag_objects first"
    );

    let mut caps: Caps = [0; CAPABILITY_WORDS];
    assert_eq!(potential(env, &mut caps), ERR_NONE);
    // Less the switches' class-hook capabilities (waves 54 and 55) and
    // `can_redefine_classes` (wave 57).
    caps[0] &= !(CAN_GENERATE_ALL_CLASS_HOOK_EVENTS | CAN_REDEFINE_CLASSES);
    caps[1] &= !CAN_RETRANSFORM_CLASSES;
    let mut expected = POTENTIAL;
    expected[0] &= !(CAN_GENERATE_ALL_CLASS_HOOK_EVENTS | CAN_REDEFINE_CLASSES);
    expected[1] &= !CAN_RETRANSFORM_CLASSES;
    assert_eq!(caps, expected);
    let name = "cratonvm/test/W12NativeAgent";
    let cid = loop_class(&shared, name);
    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;

    // Nothing is granted yet.
    assert_eq!(set_bp(env, jmid, 9), ERR_MUST_POSSESS_CAPABILITY);
    assert_eq!(
        set_mode(env, 1, EVENT_BREAKPOINT, 0),
        ERR_MUST_POSSESS_CAPABILITY
    );
    let tag_objects: Caps = [1, 0, 0, 0];
    assert_eq!(add(env, &tag_objects), ERR_NONE, "delivered since wave 63");
    assert_eq!(get_tag(env, 0, &mut tag), ERR_INVALID_OBJECT);
    assert_eq!(add(env, &POTENTIAL), ERR_NONE);
    assert_eq!(get_caps(env, &mut caps), ERR_NONE);
    assert_eq!(
        caps[0] & CAN_GENERATE_BREAKPOINT_EVENTS,
        CAN_GENERATE_BREAKPOINT_EVENTS
    );

    // The agent's structure, as long as `jvmti.h` makes it up to
    // `Breakpoint` (member 12).
    let callbacks = callbacks_with(&[(EVENT_BREAKPOINT, on_breakpoint as *const () as usize)]);
    let size = JInt::try_from(13 * std::mem::size_of::<usize>()).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    assert_eq!(set_mode(env, 1, 73, 0), ERR_NONE, "MonitorWait (wave 63)");
    assert_eq!(set_mode(env, 0, 73, 0), ERR_NONE);
    assert_eq!(set_mode(env, 1, 200, 0), ERR_INVALID_EVENT_TYPE);
    assert_eq!(
        set_mode(env, 1, 89, 0),
        ERR_INVALID_EVENT_TYPE,
        "past JDK 25's last"
    );
    assert_eq!(set_mode(env, 1, EVENT_BREAKPOINT, 0), ERR_NONE);

    assert_eq!(set_bp(env, jmid, 9), ERR_NONE);
    assert_eq!(set_bp(env, jmid, 9), rt::JvmtiError::Duplicate as JInt);
    assert_eq!(
        set_bp(env, jmid, 1000),
        rt::JvmtiError::InvalidLocation as JInt
    );
    assert_eq!(set_bp(env, jmid | 7, 0), ERR_INVALID_METHODID);

    let (mut mname, mut sig, mut generic) = (
        std::ptr::null_mut::<u8>(),
        std::ptr::null_mut::<u8>(),
        std::ptr::NonNull::<u8>::dangling().as_ptr(),
    );
    assert_eq!(
        method_name(env, jmid, &mut mname, &mut sig, &mut generic),
        ERR_NONE
    );
    // SAFETY: NUL-terminated strings this table allocated.
    let (n, s) = unsafe {
        (
            std::ffi::CStr::from_ptr(mname.cast()).to_owned(),
            std::ffi::CStr::from_ptr(sig.cast()).to_owned(),
        )
    };
    assert_eq!((n.to_str(), s.to_str()), (Ok("jvmtiBpLoop"), Ok("()I")));
    assert!(generic.is_null());
    assert_eq!(dealloc(env, mname), ERR_NONE);
    assert_eq!(dealloc(env, sig), ERR_NONE);
    let mut err = std::ptr::null_mut::<u8>();
    assert_eq!(error_name(env, 40, &mut err), ERR_NONE);
    // SAFETY: a NUL-terminated string this table allocated.
    let text = unsafe { std::ffi::CStr::from_ptr(err.cast()).to_owned() };
    assert_eq!(text.to_str(), Ok("JVMTI_ERROR_DUPLICATE"));
    assert_eq!(dealloc(env, err), ERR_NONE);
    assert_eq!(error_name(env, 57, &mut err), ERR_ILLEGAL_ARGUMENT);

    // The loop: eleven executions of pc 9, one C callback each.
    run_loop(&shared, cid, name, 0);
    assert_eq!(
        HITS_AT_9.load(Ordering::SeqCst),
        11,
        "one callback per execution"
    );
    assert_eq!(HITS.load(Ordering::SeqCst), 11);
    assert_eq!(
        HITS_WITH_JNI_ENV.load(Ordering::SeqCst),
        11,
        "a JNIEnv* each"
    );

    assert_eq!(clear_bp(env, jmid, 9), ERR_NONE);
    assert_eq!(clear_bp(env, jmid, 9), rt::JvmtiError::NotFound as JInt);
    assert_eq!(set_bp(env, jmid, 3), ERR_NONE);
    assert_eq!(dispose(env), ERR_NONE);
    assert_eq!(set_bp(env, jmid, 9), ERR_INVALID_ENVIRONMENT, "disposed");
    assert!(
        !shared
            .debug
            .debugger_gates
            .concerns_method(cid.as_u32(), "jvmtiBpLoop", "()I"),
        "disposing cleared the env's breakpoints"
    );
    assert!(
        shared.debug.jvmti_env.lock().c_table.native_envs.is_empty(),
        "disposing left the VM's list"
    );
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

static VM_INITS: AtomicUsize = AtomicUsize::new(0);
static PREPARED: AtomicU64 = AtomicU64::new(0);
static EXCEPTIONS: AtomicUsize = AtomicUsize::new(0);
static EXCEPTION_SEEN: std::sync::Mutex<Vec<(u64, i64, bool, u64, i64)>> =
    std::sync::Mutex::new(Vec::new());
static STARTUP_BP_HITS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn on_vm_init(_env: *mut JvmtiNativeEnv, jni_env: JNIEnv, _thread: JObject) {
    if !jni_env.is_null() {
        VM_INITS.fetch_add(1, Ordering::SeqCst);
    }
}

static VM_DEATHS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn on_vm_death(_env: *mut JvmtiNativeEnv, jni_env: JNIEnv) {
    if !jni_env.is_null() {
        VM_DEATHS.fetch_add(1, Ordering::SeqCst);
    }
}

unsafe extern "C" fn on_class_prepare(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    klass: JObject,
) {
    PREPARED.store(klass, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_exception(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    method: u64,
    location: i64,
    exception: JObject,
    catch_method: u64,
    catch_location: i64,
) {
    EXCEPTIONS.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut seen) = EXCEPTION_SEEN.lock() {
        seen.push((
            method,
            location,
            exception != 0,
            catch_method,
            catch_location,
        ));
    }
}

unsafe extern "C" fn on_startup_breakpoint(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
) {
    STARTUP_BP_HITS.fetch_add(1, Ordering::SeqCst);
}

/// A startup agent (wave 13): the env it obtains in `Agent_OnLoad` stays
/// unbound — `SetBreakpoint` is `WRONG_PHASE` although the thread can
/// reach a VM — while it negotiates capabilities and enables `VMInit`,
/// `ClassPrepare`, `Exception` and `Breakpoint`. Bound by `Vm::new`
/// (`bind_startup_envs`), it is in the start phase (wave 14): no
/// `VMInit` yet, `Exception` not armed on the VM, `SetBreakpoint` and
/// `SetEventNotificationMode` `WRONG_PHASE`, while `ClassPrepare`
/// arrives with the class's `jclass`, whose `GetClassSignature` and
/// `GetClassMethods` answer. Entering the live phase
/// (`enter_live_phase`, `set_init_level(3)`) sends exactly one `VMInit`
/// and arms the events; a breakpoint set on the method id
/// `GetClassMethods` answered fires; `Exception` arrives with the
/// exception as a reference and no known catch as method NULL /
/// location 0.
#[test]
fn a_startup_agent_gets_vm_init_class_prepare_and_exception_through_the_c_table() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    VM_INITS.store(0, Ordering::SeqCst);
    VM_DEATHS.store(0, Ordering::SeqCst);
    PREPARED.store(0, Ordering::SeqCst);
    EXCEPTIONS.store(0, Ordering::SeqCst);
    STARTUP_BP_HITS.store(0, Ordering::SeqCst);
    if let Ok(mut seen) = EXCEPTION_SEEN.lock() {
        seen.clear();
    }

    // `Agent_OnLoad`.
    let mut slot: Option<*mut c_void> = None;
    let envs = collect_startup_envs(|| {
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
            unsafe { std::mem::transmute(function(env, 142)) };
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
            unsafe { std::mem::transmute(function(env, 38)) };
        assert_eq!(add(env, &POTENTIAL), ERR_NONE);
        let callbacks = callbacks_with(&[
            (EVENT_VM_INIT, on_vm_init as *const () as usize),
            (EVENT_VM_DEATH, on_vm_death as *const () as usize),
            (EVENT_CLASS_PREPARE, on_class_prepare as *const () as usize),
            (EVENT_EXCEPTION, on_exception as *const () as usize),
            (
                EVENT_BREAKPOINT,
                on_startup_breakpoint as *const () as usize,
            ),
        ]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        for event in [
            EVENT_VM_INIT,
            EVENT_VM_DEATH,
            EVENT_CLASS_PREPARE,
            EVENT_EXCEPTION,
            EVENT_BREAKPOINT,
        ] {
            assert_eq!(
                set_mode(env, 1, event, 0),
                ERR_NONE,
                "event {event} at OnLoad"
            );
        }
        assert_eq!(set_bp(env, 1 << 32, 0), ERR_WRONG_PHASE, "no VM yet");
        slot = Some(env);
    });
    let env = slot.expect("the agent obtained an env");
    assert_eq!(envs, vec![env as usize]);
    assert!(
        shared.debug.jvmti_env.lock().c_table.native_envs.is_empty(),
        "not bound to the VM the thread reaches"
    );
    assert!(!shared.debug.debugger_gates.native_class_prepare_armed());

    // `Vm::new`: bound, in the start phase (past `System.initPhase1`, so
    // the start phase with `DebuggerGates::c_jvmti_phases` too, wave 55;
    // its primordial phase is
    // `a_startup_agent_gets_vm_start_after_the_primordial_phase`).
    shared.raise_vm_init_level(2);
    shared.debug.jvmti_env.lock().c_table.startup_native_envs = envs;
    bind_startup_envs(&shared);
    assert_eq!(VM_INITS.load(Ordering::SeqCst), 0, "no VMInit before live");
    assert!(shared.debug.debugger_gates.jvmti_start_phase());
    assert_eq!(
        shared.debug.jvmti_env.lock().c_table.native_envs,
        vec![env as usize]
    );
    assert!(shared.debug.debugger_gates.native_class_prepare_armed());
    assert!(
        !rt::exception_event_enabled_for_vm(vm, 3),
        "Exception is not posted in the start phase"
    );
    // SAFETY (both transmutes): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 38)) };
    assert_eq!(
        set_mode(env, 0, EVENT_EXCEPTION, 0),
        ERR_WRONG_PHASE,
        "OnLoad or live only"
    );

    // `ClassPrepare` (a start-phase event), then the class functions
    // (start or live) on its `jclass`.
    let name = "cratonvm/test/W13StartupAgent";
    let cid = loop_class(&shared, name);
    // Prepared, as the VM marks a class before it reports it (wave 52:
    // `GetClassMethods` refuses one that is not).
    if let Some(class) = shared.classes.class_manager.write().get_class_mut(cid) {
        class.state = crate::classloading::ClassState::Prepared;
    }
    crate::debug::class_prepared_on_thread(&shared, 0, cid);
    let klass = PREPARED.load(Ordering::SeqCst);
    assert_eq!(klass, jni::class_id_to_jclass(cid));
    // Once per class (wave 15): a class prepared again (an
    // initialization retried after a supertype's link failure) is not
    // reported again.
    PREPARED.store(0, Ordering::SeqCst);
    crate::debug::class_prepared_on_thread(&shared, 0, cid);
    assert_eq!(PREPARED.load(Ordering::SeqCst), 0, "reported once");
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let class_sig: extern "C" fn(*mut c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 48)) };
    let class_methods: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut *mut u64) -> JInt =
        unsafe { std::mem::transmute(function(env, 52)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let mut sig = std::ptr::null_mut::<u8>();
    assert_eq!(
        class_sig(env, klass, &mut sig, std::ptr::null_mut()),
        ERR_NONE
    );
    // SAFETY: a NUL-terminated string this table allocated.
    let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()).to_owned() };
    assert_eq!(text.to_str(), Ok("Lcratonvm/test/W13StartupAgent;"));
    assert_eq!(dealloc(env, sig), ERR_NONE);
    assert_eq!(
        class_sig(env, 0, &mut sig, std::ptr::null_mut()),
        ERR_INVALID_CLASS
    );
    let (mut count, mut methods) = (0, std::ptr::null_mut::<u64>());
    assert_eq!(
        class_methods(env, klass, &mut count, &mut methods),
        ERR_NONE
    );
    assert_eq!(count, 1);
    // SAFETY: a block of `count` ids this table allocated.
    let jmid = unsafe { *methods };
    assert_eq!(jmid, u64::from(cid.as_u32()) << 32);
    assert_eq!(dealloc(env, methods.cast::<u8>()), ERR_NONE);
    assert_eq!(set_bp(env, jmid, 9), ERR_WRONG_PHASE, "live phase only");

    // `set_init_level(3)`: the live phase, one `VMInit`, however often
    // the level is raised.
    enter_live_phase(&shared);
    enter_live_phase(&shared);
    assert_eq!(
        VM_INITS.load(Ordering::SeqCst),
        1,
        "one VMInit with a JNIEnv*"
    );
    assert!(!shared.debug.debugger_gates.jvmti_start_phase());
    assert!(
        rt::exception_event_enabled_for_vm(vm, 3),
        "Exception armed on the VM's manager for the unwinder"
    );
    assert_eq!(set_bp(env, jmid, 9), ERR_NONE);
    run_loop(&shared, cid, name, 0);
    assert_eq!(STARTUP_BP_HITS.load(Ordering::SeqCst), 11);

    // `Exception`, as the unwinder posts it (uncaught: 0 / -1).
    let thrown = shared.mem.heap.alloc_object(cid, 0);
    rt::fire_exception_for_vm(
        vm,
        0,
        jmid,
        12,
        // Cast: the object's address, as the unwinder passes it
        thrown.as_ptr() as usize as u64,
        0,
        -1,
    );
    assert_eq!(EXCEPTIONS.load(Ordering::SeqCst), 1);
    assert_eq!(
        EXCEPTION_SEEN
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default(),
        vec![(jmid, 12, true, 0, 0)]
    );

    // `Drop for Vm`: one `VMDeath`, the agents unloaded once.
    shut_down_agents(&shared, 0);
    assert_eq!(VM_DEATHS.load(Ordering::SeqCst), 1);
    assert!(shared
        .debug
        .jvmti_env
        .lock()
        .c_table
        .agent_registry
        .agents()
        .is_empty());

    // Disposed: no more deliveries, the gate is down.
    assert_eq!(dispose(env), ERR_NONE);
    assert!(!shared.debug.debugger_gates.native_class_prepare_armed());
    assert!(
        !rt::exception_event_enabled_for_vm(vm, 0),
        "disposing took Exception off the VM (wave 14)"
    );
    crate::debug::class_prepared_on_thread(&shared, 0, cid);
    // Cast: the object's address, as the unwinder passes it
    let address = thrown.as_ptr() as usize as u64;
    rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
    assert_eq!(EXCEPTIONS.load(Ordering::SeqCst), 1);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// The counter a phase test's env keeps in its environment-local
/// storage (wave 55): no test static.
fn phase_counter<'a>(env: *mut JvmtiNativeEnv) -> &'a std::sync::atomic::AtomicU64 {
    // SAFETY: an env this table handed out, whose local storage the
    // test set to a counter that outlives every delivery.
    let storage = unsafe { env_at(env as usize) }.state.lock().local_storage;
    // SAFETY: as above.
    unsafe { &*(storage as *const std::sync::atomic::AtomicU64) }
}

/// `VMStart`: one count, plus the phase it sees times 256.
unsafe extern "C" fn on_counted_vm_start(env: *mut JvmtiNativeEnv, _jni_env: JNIEnv) {
    let mut phase: JInt = 0;
    assert_eq!(get_phase(env, &mut phase), ERR_NONE);
    let phase = u64::try_from(phase).unwrap_or(0);
    phase_counter(env).fetch_add(1 + (phase << 8), Ordering::SeqCst);
}

/// `ClassPrepare`: `1 << 32`.
unsafe extern "C" fn on_counted_class_prepare(
    env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _class: JObject,
) {
    phase_counter(env).fetch_add(1 << 32, Ordering::SeqCst);
}

/// Interpreter round i2 wave 55, lane L1, with
/// `DebuggerGates::c_jvmti_phases` (the test asserts nothing without
/// it): a startup agent's env bound by `Vm::new` is in the primordial
/// phase (`GetPhase` 2) and gets no `ClassPrepare`; init level 2 (after
/// `System.initPhase1`) enters the start phase and sends one `VMStart`,
/// which sees phase 6, however often the level is raised; a class
/// prepared then is reported; level 3 is the live phase; after the
/// `VMDeath` callbacks, the dead phase.
#[test]
fn a_startup_agent_gets_vm_start_after_the_primordial_phase() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    if !shared.debug.debugger_gates.c_jvmti_phases() {
        return;
    }
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let counter = std::sync::atomic::AtomicU64::new(0);
    let mut slot: Option<*mut c_void> = None;
    let envs = collect_startup_envs(|| {
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // SAFETY (every transmute below): the slot's `jvmti.h` signature.
        let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
            unsafe { std::mem::transmute(function(env, 2)) };
        let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
            unsafe { std::mem::transmute(function(env, 122)) };
        let set_storage: extern "C" fn(*mut c_void, usize) -> JInt =
            unsafe { std::mem::transmute(function(env, 148)) };
        let callbacks = callbacks_with(&[
            (EVENT_VM_START, on_counted_vm_start as *const () as usize),
            (
                EVENT_CLASS_PREPARE,
                on_counted_class_prepare as *const () as usize,
            ),
        ]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        // Cast: the counter's address, the env's local storage
        let address = &counter as *const std::sync::atomic::AtomicU64 as usize;
        assert_eq!(set_storage(env, address), ERR_NONE);
        assert_eq!(
            set_mode(env, 1, EVENT_VM_START, 0),
            ERR_NONE,
            "VMStart at OnLoad"
        );
        assert_eq!(set_mode(env, 1, EVENT_CLASS_PREPARE, 0), ERR_NONE);
        slot = Some(env);
    });
    let env = slot.expect("the agent obtained an env");
    let phase_of = |env: *mut c_void| {
        let mut phase: JInt = 0;
        assert_eq!(get_phase(env.cast(), &mut phase), ERR_NONE);
        phase
    };

    // `Vm::new`: bound, primordial; a class prepared now is not reported.
    shared.debug.jvmti_env.lock().c_table.startup_native_envs = envs;
    bind_startup_envs(&shared);
    assert_eq!(phase_of(env), 2, "primordial");
    assert!(!shared.debug.debugger_gates.native_class_prepare_armed());
    let early = loop_class(&shared, "cratonvm/test/W55Primordial");
    crate::debug::class_prepared_on_thread(&shared, 0, early);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "nothing in the primordial phase"
    );
    // SAFETY: the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    assert_eq!(
        set_mode(env, 0, EVENT_CLASS_PREPARE, 0),
        ERR_WRONG_PHASE,
        "OnLoad or live only"
    );

    // Level 2: the start phase, one `VMStart` seeing it.
    shared.raise_vm_init_level(2);
    shared.raise_vm_init_level(2);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1 + (6 << 8),
        "one VMStart, in phase 6"
    );
    assert_eq!(phase_of(env), 6, "start");
    assert!(shared.debug.debugger_gates.native_class_prepare_armed());
    let later = loop_class(&shared, "cratonvm/test/W55Start");
    crate::debug::class_prepared_on_thread(&shared, 0, later);
    assert_eq!(
        counter.load(Ordering::SeqCst) >> 32,
        1,
        "ClassPrepare in the start phase"
    );

    // Level 3: live.
    shared.raise_vm_init_level(3);
    assert_eq!(phase_of(env), 4, "live");
    assert_eq!(set_mode(env, 1, EVENT_VM_START, 0), ERR_NONE);

    // `Drop for Vm`: dead after `VMDeath`.
    shut_down_agents(&shared, 0);
    assert_eq!(phase_of(env), 8, "dead");
    assert_eq!(
        counter.load(Ordering::SeqCst) & 0xFF,
        1,
        "one VMStart in all"
    );
    // SAFETY: the slot's `jvmti.h` signature.
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Interpreter round i2 wave 55, lane L1, with
/// `DebuggerGates::c_lambda_classes` (the test asserts nothing without
/// it): a lambda proxy class the VM minted outside the class store is
/// listed by `GetLoadedClasses` and answers HotSpot's facts of a lambda's
/// hidden class (measured, `L1W55JvmtiLambdaClass`): its signature after
/// its host, `0x1030`, status 7, its functional interface, neither an
/// interface nor an array, version 69, no source file.
#[test]
fn a_lambda_proxy_class_answers_the_class_functions() {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    if !shared.debug.debugger_gates.c_lambda_classes() {
        return;
    }
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let host = loop_class(&shared, "cratonvm/test/W55Host");
    let iface = loop_class(&shared, "cratonvm/test/W55Iface");
    let proxy = shared.alloc_lambda_proxy_id();
    shared.classes.lambda_proxies.write().insert(
        proxy,
        Arc::new(LambdaCallSite {
            functional_interface_id: Some(iface),
            functional_interface: "cratonvm/test/W55Iface".into(),
            sam_method_name: "run".into(),
            sam_descriptor: "()V".into(),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: "cratonvm/test/W55Host".into(),
                member_name: "lambda$main$0".into(),
                descriptor: "()V".into(),
            },
            instantiated_descriptor: "()V".into(),
            capture_types: Vec::new(),
            proxy_class_id: proxy,
            serializable_flag: false,
        }),
    );
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let env = env.cast::<JvmtiNativeEnv>();
    let klass = jni::class_id_to_jclass(proxy);

    let (mut count, mut classes) = (0, std::ptr::null_mut::<JObject>());
    assert_eq!(get_loaded_classes(env, &mut count, &mut classes), ERR_NONE);
    // SAFETY: a block of `count` handles this table allocated.
    let listed =
        unsafe { std::slice::from_raw_parts(classes, usize::try_from(count).unwrap_or(0)) }
            .contains(&klass);
    assert!(listed, "GetLoadedClasses lists the proxy");
    // SAFETY: a block this table allocated.
    unsafe { free_block(classes.cast()) };

    let (mut sig, mut generic) = (std::ptr::null_mut::<u8>(), std::ptr::null_mut::<u8>());
    assert_eq!(
        get_class_signature(env, klass, &mut sig, &mut generic),
        ERR_NONE
    );
    // SAFETY: a NUL-terminated string this table allocated.
    let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()).to_owned() };
    let expected = format!("Lcratonvm/test/W55Host$$Lambda.0x{:016x};", proxy.as_u32());
    assert_eq!(text.to_str(), Ok(expected.as_str()));
    assert!(generic.is_null(), "no generic signature");
    // SAFETY: a block this table allocated.
    unsafe { free_block(sig) };

    let mut value: JInt = 0;
    assert_eq!(get_class_modifiers(env, klass, &mut value), ERR_NONE);
    assert_eq!(value, 0x1030);
    assert_eq!(get_class_status(env, klass, &mut value), ERR_NONE);
    assert_eq!(value, 7);
    let mut flag: u8 = 1;
    assert_eq!(is_interface(env, klass, &mut flag), ERR_NONE);
    assert_eq!(flag, 0);
    flag = 1;
    assert_eq!(is_array_class(env, klass, &mut flag), ERR_NONE);
    assert_eq!(flag, 0);
    let (mut minor, mut major) = (-1, -1);
    assert_eq!(
        get_class_version_numbers(env, klass, &mut minor, &mut major),
        ERR_NONE
    );
    assert_eq!((minor, major), (0, 69));
    let mut interfaces = std::ptr::null_mut::<JObject>();
    assert_eq!(
        get_implemented_interfaces(env, klass, &mut count, &mut interfaces),
        ERR_NONE
    );
    assert_eq!(count, 1);
    // SAFETY: a block of one handle this table allocated.
    assert_eq!(unsafe { *interfaces }, jni::class_id_to_jclass(iface));
    // SAFETY: a block this table allocated.
    unsafe { free_block(interfaces.cast()) };
    let _ = host;
    assert_eq!(dispose_environment(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Interpreter round i2 wave 56, lane L1, with
/// `DebuggerGates::c_lambda_methods` (the test asserts nothing without
/// it): `GetClassMethods` of a capturing lambda proxy lists its
/// constructor and its functional method, and the method functions answer
/// HotSpot's facts of each (measured, `L1W56JvmtiLambdaMethods`).
#[test]
fn a_lambda_proxy_class_answers_its_methods() {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    if !shared.debug.debugger_gates.c_lambda_methods() {
        return;
    }
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let iface = loop_class(&shared, "cratonvm/test/W56Function");
    let _host = loop_class(&shared, "cratonvm/test/W56Host");
    let proxy = shared.alloc_lambda_proxy_id();
    shared.classes.lambda_proxies.write().insert(
        proxy,
        Arc::new(LambdaCallSite {
            functional_interface_id: Some(iface),
            functional_interface: "cratonvm/test/W56Function".into(),
            sam_method_name: "apply".into(),
            sam_descriptor: "(Ljava/lang/Object;)Ljava/lang/Object;".into(),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: "cratonvm/test/W56Host".into(),
                member_name: "lambda$main$1".into(),
                descriptor: "(Ljava/lang/String;JLjava/lang/Integer;)Ljava/lang/String;".into(),
            },
            instantiated_descriptor: "(Ljava/lang/Integer;)Ljava/lang/String;".into(),
            capture_types: vec!['L', 'J'],
            proxy_class_id: proxy,
            serializable_flag: false,
        }),
    );
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let env = env.cast::<JvmtiNativeEnv>();
    let klass = jni::class_id_to_jclass(proxy);

    let (mut count, mut methods) = (0, std::ptr::null_mut::<u64>());
    assert_eq!(
        get_class_methods(env, klass, &mut count, &mut methods),
        ERR_NONE
    );
    assert_eq!(count, 2, "the constructor and the functional method");
    // SAFETY: a block of `count` ids this table allocated.
    let ids = unsafe { std::slice::from_raw_parts(methods, 2) }.to_vec();
    // SAFETY: a block this table allocated.
    unsafe { free_block(methods.cast()) };
    let expected = [
        ("<init>", "(Ljava/lang/String;J)V", 0x2, 4),
        ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;", 0x1, 2),
    ];
    for (&id, (name, descriptor, modifiers, slots)) in ids.iter().zip(expected) {
        let (mut n, mut s, mut g) = (
            std::ptr::null_mut::<u8>(),
            std::ptr::null_mut::<u8>(),
            std::ptr::null_mut::<u8>(),
        );
        assert_eq!(get_method_name(env, id, &mut n, &mut s, &mut g), ERR_NONE);
        // SAFETY: NUL-terminated strings this table allocated.
        let (n_text, s_text) = unsafe {
            (
                std::ffi::CStr::from_ptr(n.cast()).to_owned(),
                std::ffi::CStr::from_ptr(s.cast()).to_owned(),
            )
        };
        assert_eq!(n_text.to_str(), Ok(name));
        assert_eq!(s_text.to_str(), Ok(descriptor));
        assert!(g.is_null(), "no generic signature");
        // SAFETY: blocks this table allocated.
        unsafe {
            free_block(n);
            free_block(s);
        }
        let mut declaring: JObject = 0;
        assert_eq!(
            get_method_declaring_class(env, id, &mut declaring),
            ERR_NONE
        );
        assert_eq!(declaring, klass);
        let mut value: JInt = -1;
        assert_eq!(get_method_modifiers(env, id, &mut value), ERR_NONE);
        assert_eq!(value, modifiers, "{name}");
        assert_eq!(get_arguments_size(env, id, &mut value), ERR_NONE);
        assert_eq!(value, slots, "{name}");
        assert_eq!(get_max_locals(env, id, &mut value), ERR_NONE);
        assert_eq!(value, slots, "{name}");
        let mut flag: u8 = 1;
        assert_eq!(is_method_native(env, id, &mut flag), ERR_NONE);
        assert_eq!(flag, 0);
        // Wave 57 (`CRATONVM_JVMTI_C_LAMBDA_CODE`, on): the spun code's
        // range (it was `JVMTI_ERROR_ABSENT_INFORMATION`).
        let (mut start, mut end) = (-1i64, -1i64);
        assert_eq!(
            get_method_location(env, id, &mut start, &mut end),
            ERR_NONE,
            "{name}"
        );
        assert_eq!(start, 0, "{name}");
        assert!(end > 0, "{name}: the code ends at {end}");
    }
    let past = (u64::from(proxy.as_u32()) << 32) | 2;
    let mut value: JInt = -1;
    assert_eq!(
        get_method_modifiers(env, past, &mut value),
        ERR_INVALID_METHODID,
        "no third method"
    );
    assert_eq!(dispose_environment(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Interpreter round i2 wave 56, lane L1: the CPU-time functions answer
/// HotSpot's (measured, `L1W56JvmtiThreadTimes`): 99 without the
/// capability, then the total-CPU timer and the calling thread's clock;
/// `IsMethodObsolete` refuses an id that names no method.
#[test]
fn the_cpu_time_functions_answer_with_their_capabilities() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let env = env.cast::<JvmtiNativeEnv>();
    let mut nanos: i64 = -1;
    assert_eq!(
        get_current_thread_cpu_time(env, &mut nanos),
        ERR_MUST_POSSESS_CAPABILITY
    );
    assert_eq!(
        get_thread_cpu_time(env, 0, &mut nanos),
        ERR_MUST_POSSESS_CAPABILITY
    );
    let wanted: [u32; CAPABILITY_WORDS] = [
        CAN_GET_CURRENT_THREAD_CPU_TIME | CAN_GET_THREAD_CPU_TIME,
        0,
        0,
        0,
    ];
    assert_eq!(add_capabilities(env, &wanted), ERR_NONE);
    let mut info = TimerInfoRow {
        max_value: 0,
        may_skip_forward: 1,
        may_skip_backward: 1,
        kind: 0,
        reserved1: 0,
        reserved2: 0,
    };
    assert_eq!(get_thread_cpu_timer_info(env, &mut info), ERR_NONE);
    assert_eq!(
        (
            info.max_value,
            info.may_skip_forward,
            info.may_skip_backward,
            info.kind
        ),
        (-1, 0, 0, TIMER_TOTAL_CPU)
    );
    assert_eq!(
        get_current_thread_cpu_timer_info(env, std::ptr::null_mut()),
        ERR_NULL_POINTER
    );
    if current_thread_cpu_nanos().is_some() {
        assert_eq!(get_current_thread_cpu_time(env, &mut nanos), ERR_NONE);
        assert!(nanos >= 0);
        assert_eq!(get_thread_cpu_time(env, 0, &mut nanos), ERR_NONE);
    }
    assert_eq!(
        get_thread_cpu_time(env, 0, std::ptr::null_mut()),
        ERR_NULL_POINTER
    );
    let mut flag: u8 = 2;
    assert_eq!(
        is_method_obsolete(env, u64::from(u32::MAX - 7) << 32, &mut flag),
        ERR_INVALID_METHODID
    );
    assert_eq!(dispose_environment(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Interpreter round i2 wave 61, lane L1: the startup functions answer
/// HotSpot's (MEASURED, `L1W61JvmtiAgentStartup`): the VM's own
/// properties only, `WRONG_PHASE` for `SetSystemProperty` in the live
/// phase, the elapsed timer, `ILLEGAL_ARGUMENT` for a search segment
/// that is no jar; and frame control answers `NOT_AVAILABLE` while its
/// switch is off.
#[test]
fn the_wave_61_startup_functions_answer_through_the_c_table() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let env = env.cast::<JvmtiNativeEnv>();
    let read = |key: &str| -> Result<String, JInt> {
        let key = std::ffi::CString::new(key).unwrap();
        let mut value: *mut u8 = std::ptr::null_mut();
        let code = get_system_property(env, key.as_ptr(), &mut value);
        if code != ERR_NONE {
            return Err(code);
        }
        // SAFETY: a NUL-terminated `Allocate` block of this table.
        let text = unsafe { std::ffi::CStr::from_ptr(value.cast()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(deallocate(env, value), ERR_NONE);
        Ok(text)
    };
    assert_eq!(read("java.vm.specification.version"), Ok("25".to_string()));
    assert_eq!(read("user.dir"), Err(ERR_NOT_AVAILABLE));
    assert_eq!(read("l1w61.missing"), Err(ERR_NOT_AVAILABLE));
    assert_eq!(
        get_system_property(env, std::ptr::null(), &mut std::ptr::null_mut()),
        ERR_NULL_POINTER
    );
    let mut count: JInt = -1;
    let mut keys: *mut *mut u8 = std::ptr::null_mut();
    assert_eq!(get_system_properties(env, &mut count, &mut keys), ERR_NONE);
    let listed: Vec<String> = (0..usize::try_from(count).unwrap())
        .map(|i| {
            // SAFETY: `count` blocks the call allocated.
            let key = unsafe { *keys.add(i) };
            let text = unsafe { std::ffi::CStr::from_ptr(key.cast()) }
                .to_string_lossy()
                .into_owned();
            assert_eq!(deallocate(env, key), ERR_NONE);
            text
        })
        .collect();
    assert_eq!(deallocate(env, keys.cast()), ERR_NONE);
    assert!(listed.iter().any(|k| k == "java.home"), "{listed:?}");
    assert!(
        !listed.iter().any(|k| k == "user.dir" || k == "os.name"),
        "{listed:?}"
    );
    let x = std::ffi::CString::new("x").unwrap();
    assert_eq!(
        set_system_property(env, std::ptr::null(), x.as_ptr()),
        ERR_WRONG_PHASE
    );
    let mut info = TimerInfoRow {
        max_value: 0,
        may_skip_forward: 1,
        may_skip_backward: 1,
        kind: 0,
        reserved1: 0,
        reserved2: 0,
    };
    assert_eq!(get_timer_info(env, &mut info), ERR_NONE);
    assert_eq!(
        (
            info.max_value,
            info.may_skip_forward,
            info.may_skip_backward,
            info.kind
        ),
        (-1, 0, 0, TIMER_ELAPSED)
    );
    assert_eq!(get_timer_info(env, std::ptr::null_mut()), ERR_NULL_POINTER);
    // `SetEventNotificationMode` as HotSpot reads its arguments.
    assert_eq!(
        set_event_notification_mode(env, 7, EVENT_THREAD_START, 0),
        ERR_NONE,
        "mode 7 disables"
    );
    assert_eq!(
        set_event_notification_mode(env, 1, 49, 0),
        ERR_NONE,
        "an extension event"
    );
    assert_eq!(
        set_event_notification_mode(env, 1, 72, 0),
        ERR_NONE,
        "a reserved event"
    );
    assert_eq!(
        set_event_notification_mode(env, 1, 46, 0),
        ERR_INVALID_EVENT_TYPE
    );
    let wanted: [u32; CAPABILITY_WORDS] = [CAN_GENERATE_BREAKPOINT_EVENTS, 0, 0, 0];
    assert_eq!(add_capabilities(env, &wanted), ERR_NONE);
    assert_eq!(set_breakpoint(env, 0, 0), ERR_INVALID_METHODID);
    assert_eq!(clear_breakpoint(env, 0, 0), ERR_INVALID_METHODID);
    let plain =
        std::env::temp_dir().join(format!("l1w61-not-a-jar-{}.txt", std::process::id()));
    std::fs::write(&plain, b"not a jar").unwrap();
    let plain_c = std::ffi::CString::new(plain.to_string_lossy().into_owned()).unwrap();
    assert_eq!(
        add_to_bootstrap_class_loader_search(env, plain_c.as_ptr()),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(
        add_to_system_class_loader_search(env, plain_c.as_ptr()),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(
        add_to_system_class_loader_search(env, std::ptr::null()),
        ERR_NULL_POINTER
    );
    let _ = std::fs::remove_file(&plain);
    let mut hash: JInt = 0;
    assert_eq!(get_object_hash_code(env, 0, &mut hash), ERR_INVALID_OBJECT);
    let mut size: i64 = 0;
    assert_eq!(get_object_size(env, 0, &mut size), ERR_INVALID_OBJECT);
    if !shared.debug.debugger_gates.c_frame_control() {
        assert_eq!(pop_frame(env, 0), ERR_NOT_AVAILABLE);
        assert_eq!(force_early_return_int(env, 0, 1), ERR_NOT_AVAILABLE);
        assert_eq!(clear_all_frame_pops(env, 0), ERR_NOT_AVAILABLE);
        let mut caps = [0u32; CAPABILITY_WORDS];
        assert_eq!(get_potential_capabilities(env, &mut caps), ERR_NONE);
        assert_eq!(
            (caps[0] & CAN_POP_FRAME, caps[1] & CAN_FORCE_EARLY_RETURN),
            (0, 0)
        );
    }
    assert_eq!(dispose_environment(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Wave 61: the OnLoad phase's VM properties: the identity and path keys
/// with `SharedVm::new`'s initial values, the `-D` ones writable, the
/// internal ones left out.
#[test]
fn the_onload_vm_properties_are_hotspots_list() {
    let mut config = crate::config::VmConfig::default();
    config.system_properties = vec![
        ("l1w61.user".to_string(), "u".to_string()),
        ("jdk.boot.class.path.append".to_string(), "b".to_string()),
    ];
    let props = onload_vm_properties(&config);
    let find = |key: &str| props.iter().find(|(k, _, _)| k == key).cloned();
    assert_eq!(
        find("java.vm.specification.version"),
        Some((
            "java.vm.specification.version".to_string(),
            "25".to_string(),
            false
        ))
    );
    assert_eq!(find("java.vm.version").map(|p| p.2), Some(false));
    assert_eq!(find("java.library.path").map(|p| p.2), Some(true));
    assert_eq!(find("java.home").map(|p| p.2), Some(true));
    assert_eq!(
        find("l1w61.user"),
        Some(("l1w61.user".to_string(), "u".to_string(), true))
    );
    assert_eq!(find("jdk.boot.class.path.append"), None);
    assert_eq!(find("java.vm.compressedOopsMode"), None);
    assert_eq!(find("user.dir"), None);
}

/// The i53 residual: an explicit `-Dsun.boot.library.path` is appended
/// to the default, as HotSpot does; `-Djava.library.path` replaces it.
#[test]
fn i53r_an_explicit_boot_library_path_is_appended() {
    let mut config = crate::config::VmConfig::default();
    config.system_properties = vec![
        ("sun.boot.library.path".to_string(), "i53r-boot".to_string()),
        ("java.library.path".to_string(), "i53r-lib".to_string()),
    ];
    let props = onload_vm_properties(&config);
    let find = |key: &str| props.iter().find(|(k, _, _)| k == key).map(|p| p.1.clone());
    let home = crate::vm::vm_init::initial_java_home(&config);
    let sep = if cfg!(windows) { ';' } else { ':' };
    assert_eq!(
        find("sun.boot.library.path"),
        Some(format!(
            "{}{sep}i53r-boot",
            crate::vm::vm_init::initial_sun_boot_library_path(&home)
        ))
    );
    assert_eq!(find("java.library.path"), Some("i53r-lib".to_string()));
}

/// Interpreter round i7 wave 135, lane L1b (`CRATONVM_VM_PROPERTY_LIST_ORDER`):
/// the OnLoad list is in HotSpot's measured order, a `-D` of a list key
/// keeps the key's place, and the other `-D` ones follow the list.
#[test]
fn i135_l1b_the_onload_vm_properties_are_in_hotspots_order() {
    let mut config = crate::config::VmConfig::default();
    config.vm_property_list_order = true;
    config.system_properties = vec![
        ("l1b.user".to_string(), "u".to_string()),
        ("java.class.path".to_string(), "cp".to_string()),
    ];
    let props = onload_vm_properties(&config);
    let keys: Vec<&str> = props.iter().map(|(k, _, _)| k.as_str()).collect();
    let list: Vec<&str> = cratonvm_types::vm_properties::HOTSPOT_LIST_ORDER
        .iter()
        .copied()
        .filter(|k| *k != "java.vm.compressedOopsMode")
        .collect();
    assert_eq!(&keys[..list.len()], &list[..]);
    assert_eq!(&keys[list.len()..], &["l1b.user"]);
    config.vm_property_list_order = false;
    let old = onload_vm_properties(&config);
    assert_eq!(old.last().map(|p| p.0.as_str()), Some("java.class.path"));
}

static THREAD_BP_HITS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn on_thread_breakpoint(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
) {
    THREAD_BP_HITS.fetch_add(1, Ordering::SeqCst);
}

/// Per-thread enabling (wave 13): `Breakpoint` enabled for one
/// `jthread` reaches that thread only; `VMInit` is not controllable per
/// thread; a handle that names no VM thread is `INVALID_THREAD`.
#[test]
fn an_event_enabled_for_one_thread_reaches_that_thread_only() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    THREAD_BP_HITS.store(0, Ordering::SeqCst);

    let name = "cratonvm/test/W13ThreadAgent";
    let cid = loop_class(&shared, name);
    let mirror = shared.mem.heap.alloc_object(cid, 0);
    shared
        .threads
        .thread_registry
        .register(ThreadId(5), "w13-t5", Some(mirror));
    let jthread = jni::obj_to_jobject(mirror);

    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 38)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    assert_eq!(add(env, &POTENTIAL), ERR_NONE);
    let callbacks =
        callbacks_with(&[(EVENT_BREAKPOINT, on_thread_breakpoint as *const () as usize)]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );

    assert_eq!(
        set_mode(env, 1, EVENT_VM_INIT, jthread),
        ERR_ILLEGAL_ARGUMENT
    );
    // Wave 53 (round i2, measured): `ThreadStart` is global on HotSpot,
    // `ThreadEnd` is not.
    assert_eq!(
        set_mode(env, 1, EVENT_THREAD_START, jthread),
        ERR_ILLEGAL_ARGUMENT
    );
    assert_eq!(set_mode(env, 1, EVENT_THREAD_END, jthread), ERR_NONE);
    assert_eq!(set_mode(env, 0, EVENT_THREAD_END, jthread), ERR_NONE);
    let stranger = jni::obj_to_jobject(shared.mem.heap.alloc_object(cid, 0));
    assert_eq!(
        set_mode(env, 1, EVENT_BREAKPOINT, stranger),
        ERR_INVALID_THREAD
    );
    assert_eq!(set_mode(env, 1, EVENT_BREAKPOINT, jthread), ERR_NONE);
    let jmid = u64::from(cid.as_u32()) << 32;
    assert_eq!(set_bp(env, jmid, 9), ERR_NONE);

    run_loop(&shared, cid, name, 6);
    assert_eq!(THREAD_BP_HITS.load(Ordering::SeqCst), 0, "another thread");
    run_loop(&shared, cid, name, 5);
    assert_eq!(
        THREAD_BP_HITS.load(Ordering::SeqCst),
        11,
        "the enabled thread"
    );
    assert_eq!(set_mode(env, 0, EVENT_BREAKPOINT, jthread), ERR_NONE);
    run_loop(&shared, cid, name, 5);
    assert_eq!(THREAD_BP_HITS.load(Ordering::SeqCst), 11, "disabled again");

    assert_eq!(dispose(env), ERR_NONE);
    shared.threads.thread_registry.mark_dead(ThreadId(5));
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Wave 19: the handle checks answer what HotSpot's do. A terminated
/// thread is `THREAD_NOT_ALIVE` (it was accepted, and the event enabled
/// for a thread that could never post it); a handle that is neither a
/// `jclass` nor a `Class` mirror is `INVALID_CLASS` (JNI's decode fell back
/// to the handle's low 32 bits, so this one, a class id without the
/// `jclass` tag, named that class).
#[test]
fn a_dead_thread_and_a_non_class_handle_are_refused_as_hotspot_refuses_them() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));

    let name = "cratonvm/test/W19HandleChecks";
    let cid = loop_class(&shared, name);
    let mirror = shared.mem.heap.alloc_object(cid, 0);
    shared
        .threads
        .thread_registry
        .register(ThreadId(7), "w19-t7", Some(mirror));
    let jthread = jni::obj_to_jobject(mirror);

    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let signature: extern "C" fn(*mut c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 48)) };
    let status: extern "C" fn(*mut c_void, JObject, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 49)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };

    // `ClassPrepare` needs no capability.
    assert_eq!(set_mode(env, 1, EVENT_CLASS_PREPARE, jthread), ERR_NONE);
    assert_eq!(set_mode(env, 0, EVENT_CLASS_PREPARE, jthread), ERR_NONE);
    shared.threads.thread_registry.mark_dead(ThreadId(7));
    assert_eq!(
        set_mode(env, 1, EVENT_CLASS_PREPARE, jthread),
        ERR_THREAD_NOT_ALIVE
    );

    let mut sig = std::ptr::null_mut::<u8>();
    let klass = jni::class_id_to_jclass(cid);
    assert_eq!(
        signature(env, klass, &mut sig, std::ptr::null_mut()),
        ERR_NONE
    );
    // SAFETY: a NUL-terminated string this table allocated.
    let text = unsafe { std::ffi::CStr::from_ptr(sig.cast()).to_owned() };
    assert_eq!(text.to_str(), Ok("Lcratonvm/test/W19HandleChecks;"));
    assert_eq!(dealloc(env, sig), ERR_NONE);
    let untagged = u64::from(cid.as_u32());
    assert_eq!(
        signature(env, untagged, &mut sig, std::ptr::null_mut()),
        ERR_INVALID_CLASS
    );
    let mut st = 0;
    assert_eq!(status(env, untagged, &mut st), ERR_INVALID_CLASS);
    assert_eq!(
        status(env, jthread, &mut st),
        ERR_INVALID_CLASS,
        "an object"
    );

    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

static ENV_A_EXCEPTIONS: AtomicUsize = AtomicUsize::new(0);
static ENV_B_EXCEPTIONS: AtomicUsize = AtomicUsize::new(0);

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_exception_a(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
    _exception: JObject,
    _catch_method: u64,
    _catch_location: i64,
) {
    ENV_A_EXCEPTIONS.fetch_add(1, Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_exception_b(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
    _exception: JObject,
    _catch_method: u64,
    _catch_location: i64,
) {
    ENV_B_EXCEPTIONS.fetch_add(1, Ordering::SeqCst);
}

/// Wave 14: enabling is per env. Of two C envs with an `Exception`
/// callback, only the one that enabled the event receives it, and a Rust
/// callback on the VM manager's own table that never enabled it receives
/// nothing. The env's disable, its relinquished capability and its
/// dispose each take the event off the VM: the unwinder's exact check
/// (`exception_event_enabled_for_vm`) answers no again. Until wave 14
/// the enable stayed on the manager for the rest of the run.
#[test]
fn a_c_env_gets_only_what_it_enabled_and_its_disable_reaches_the_vm() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    ENV_A_EXCEPTIONS.store(0, Ordering::SeqCst);
    ENV_B_EXCEPTIONS.store(0, Ordering::SeqCst);
    let name = "cratonvm/test/W14TwoEnvs";
    let cid = loop_class(&shared, name);
    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;
    let thrown = shared.mem.heap.alloc_object(cid, 0);
    // Cast: the object's address, as the unwinder passes it
    let address = thrown.as_ptr() as usize as u64;

    let manager = rt::manager_for_vm(vm).expect("the fixture's manager");
    let own = Arc::new(AtomicUsize::new(0));
    let own_seen = Arc::clone(&own);
    manager
        .set_event_callbacks(EventCallbacks {
            exception: Some(Box::new(move |_, _, _, _, _, _| {
                own_seen.fetch_add(1, Ordering::SeqCst);
            })),
            ..Default::default()
        })
        .expect("installed");

    // SAFETY (every transmute below): the slot's `jvmti.h` signature;
    // both envs share the one table.
    let (_, probe) = get_env(0x3001_0200);
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(probe, 142)) };
    let relinquish: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(probe, 143)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(probe, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(probe, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(probe, 127)) };
    assert_eq!(dispose(probe), ERR_NONE);
    let mut envs = Vec::new();
    for callback in [
        on_exception_a as *const () as usize,
        on_exception_b as *const () as usize,
    ] {
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // A solo capability goes to one env at a time (HotSpot; wave 63).
        let wanted = if envs.is_empty() {
            POTENTIAL
        } else {
            without(POTENTIAL, SOLO)
        };
        assert_eq!(add(env, &wanted), ERR_NONE);
        let callbacks = callbacks_with(&[(EVENT_EXCEPTION, callback)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        envs.push(env);
    }
    let (a, b) = (envs[0], envs[1]);

    // A enables `Exception`; B enables only `FramePop` (so it has a
    // delivery env on the manager too; not `Breakpoint`, an OnLoad-solo
    // capability A holds since wave 63).
    assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
    assert_eq!(set_mode(b, 1, EVENT_FRAME_POP, 0), ERR_NONE);
    assert!(rt::exception_event_enabled_for_vm(vm, 0));
    rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
    assert_eq!(ENV_A_EXCEPTIONS.load(Ordering::SeqCst), 1);
    assert_eq!(
        ENV_B_EXCEPTIONS.load(Ordering::SeqCst),
        0,
        "B never enabled it"
    );
    assert_eq!(
        own.load(Ordering::SeqCst),
        0,
        "the manager never enabled it"
    );

    // A disables it: nothing on the VM has it enabled any more.
    assert_eq!(set_mode(a, 0, EVENT_EXCEPTION, 0), ERR_NONE);
    assert!(
        !rt::exception_event_enabled_for_vm(vm, 0),
        "the unwinder's slow path is off again"
    );
    assert!(!manager.has_exception_listener());
    rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
    assert_eq!(ENV_A_EXCEPTIONS.load(Ordering::SeqCst), 1);

    // Enabled again, then the capability relinquished.
    assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
    assert!(rt::exception_event_enabled_for_vm(vm, 0));
    let exception_capability: Caps = [CAN_GENERATE_EXCEPTION_EVENTS, 0, 0, 0];
    assert_eq!(relinquish(a, &exception_capability), ERR_NONE);
    assert!(!rt::exception_event_enabled_for_vm(vm, 0));

    // Enabled again, then the env disposed.
    assert_eq!(add(a, &POTENTIAL), ERR_NONE);
    assert_eq!(set_mode(a, 1, EVENT_EXCEPTION, 0), ERR_NONE);
    assert!(rt::exception_event_enabled_for_vm(vm, 0));
    assert_eq!(dispose(a), ERR_NONE);
    assert!(!rt::exception_event_enabled_for_vm(vm, 0));
    assert!(manager.is_event_enabled(JvmtiEventKind::FramePop, Some(0)));
    assert_eq!(dispose(b), ERR_NONE);
    assert!(
        !manager.is_event_enabled(JvmtiEventKind::FramePop, Some(0)),
        "B's dispose took its FramePop off"
    );
    assert_eq!(ENV_B_EXCEPTIONS.load(Ordering::SeqCst), 0);
    assert_eq!(own.load(Ordering::SeqCst), 0);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Wave 41 (lane L1): `GetClassSignature` and `GetMethodName` answer the
/// `Signature` attribute as the generic signature, and NULL for a class
/// or method without one (both answered NULL always).
#[test]
fn generic_signatures_are_the_signature_attributes() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let generic = loop_class(&shared, "cratonvm/test/W41Generic");
    let plain = loop_class(&shared, "cratonvm/test/W41Plain");
    {
        let mut cm = shared.classes.class_manager.write();
        let class = cm.get_class_mut(generic).expect("fabricated");
        class.signature = Some("<T:Ljava/lang/Object;>Ljava/lang/Object;".to_string());
        class.methods[0]
            .attributes
            .push(LazyAttribute::Decoded(Attribute::Signature(Arc::from(
                "<U:Ljava/lang/Object;>()I",
            ))));
    }
    // Widening: the class id's half of the jmethodID.
    let generic_jmid = u64::from(generic.as_u32()) << 32;
    let plain_jmid = u64::from(plain.as_u32()) << 32;

    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let class_sig: extern "C" fn(*mut c_void, JObject, *mut *mut u8, *mut *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 48)) };
    let method_name: extern "C" fn(
        *mut c_void,
        u64,
        *mut *mut u8,
        *mut *mut u8,
        *mut *mut u8,
    ) -> JInt = unsafe { std::mem::transmute(function(env, 64)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    // The text of an answered block, freed; `None` for NULL.
    let text = |block: *mut u8| -> Option<String> {
        if block.is_null() {
            return None;
        }
        // SAFETY: a NUL-terminated block the table allocated.
        let s = unsafe { std::ffi::CStr::from_ptr(block.cast()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(dealloc(env, block), ERR_NONE);
        Some(s)
    };

    // A non-NULL value the table must overwrite with NULL.
    let marker = std::ptr::NonNull::<u8>::dangling().as_ptr();
    let (mut sig, mut generic_out) = (std::ptr::null_mut::<u8>(), marker);
    assert_eq!(
        class_sig(
            env,
            jni::class_id_to_jclass(generic),
            &mut sig,
            &mut generic_out
        ),
        ERR_NONE
    );
    assert_eq!(text(sig).as_deref(), Some("Lcratonvm/test/W41Generic;"));
    assert_eq!(
        text(generic_out).as_deref(),
        Some("<T:Ljava/lang/Object;>Ljava/lang/Object;")
    );
    let (mut sig, mut generic_out) = (std::ptr::null_mut::<u8>(), marker);
    assert_eq!(
        class_sig(
            env,
            jni::class_id_to_jclass(plain),
            &mut sig,
            &mut generic_out
        ),
        ERR_NONE
    );
    assert_eq!(text(sig).as_deref(), Some("Lcratonvm/test/W41Plain;"));
    assert!(generic_out.is_null(), "no Signature attribute: NULL");

    let (mut name, mut desc, mut generic_out) = (
        std::ptr::null_mut::<u8>(),
        std::ptr::null_mut::<u8>(),
        marker,
    );
    assert_eq!(
        method_name(env, generic_jmid, &mut name, &mut desc, &mut generic_out),
        ERR_NONE
    );
    assert_eq!(text(name).as_deref(), Some("jvmtiBpLoop"));
    assert_eq!(text(desc).as_deref(), Some("()I"));
    assert_eq!(
        text(generic_out).as_deref(),
        Some("<U:Ljava/lang/Object;>()I")
    );
    let mut generic_out = marker;
    assert_eq!(
        method_name(
            env,
            plain_jmid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut generic_out
        ),
        ERR_NONE
    );
    assert!(generic_out.is_null(), "no Signature attribute: NULL");

    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// JVM type signatures of a class, an array and a primitive.
#[test]
fn class_signatures_follow_the_jvm_type_grammar() {
    assert_eq!(class_signature("java/lang/String"), "Ljava/lang/String;");
    assert_eq!(
        class_signature("[Ljava/lang/String;"),
        "[Ljava/lang/String;"
    );
    assert_eq!(class_signature("[I"), "[I");
    assert_eq!(class_signature("int"), "I");
    assert_eq!(class_signature("void"), "V");
}

/// Modified UTF-8, as JVMTI strings are: NUL is `C0 80`, a supplementary
/// character its two surrogates of three bytes each.
#[test]
fn strings_are_modified_utf8() {
    let mem = allocate_modified_utf8("a\u{0}\u{1F600}");
    assert!(!mem.is_null());
    // SAFETY: a NUL-terminated block allocated just above.
    let bytes = unsafe { std::ffi::CStr::from_ptr(mem.cast()).to_bytes().to_vec() };
    assert_eq!(
        bytes,
        vec![b'a', 0xC0, 0x80, 0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x80]
    );
    // SAFETY: allocated above, freed once.
    unsafe { free_block(mem) };
}

static METHOD_WATCHED: AtomicU64 = AtomicU64::new(0);
static METHOD_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static METHOD_EXITS: AtomicUsize = AtomicUsize::new(0);
static METHOD_EXIT_VALUE: AtomicU64 = AtomicU64::new(0);
static METHOD_EXIT_POPPED: AtomicUsize = AtomicUsize::new(0);

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_method_entry(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    method: u64,
) {
    if method == METHOD_WATCHED.load(Ordering::SeqCst) {
        METHOD_ENTRIES.fetch_add(1, Ordering::SeqCst);
    }
}

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_method_exit(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    method: u64,
    was_popped_by_exception: u8,
    value: u64,
) {
    if method == METHOD_WATCHED.load(Ordering::SeqCst) {
        METHOD_EXITS.fetch_add(1, Ordering::SeqCst);
        METHOD_EXIT_VALUE.store(value, Ordering::SeqCst);
        METHOD_EXIT_POPPED.store(usize::from(was_popped_by_exception), Ordering::SeqCst);
    }
}

/// Wave 17 (proposal `interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016`,
/// stage 1): a C env with `can_generate_method_entry_events` /
/// `can_generate_method_exit_events` gets `MethodEntry` and `MethodExit`
/// (the `jint` result in the `jvalue`, not popped by an exception)
/// through `jvmti.h`'s members 15 and 16, the VM runs interpreted while
/// they are enabled, and a disable or a dispose takes each off.
#[test]
fn a_c_env_gets_method_entry_and_exit_with_the_return_value() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    // `jvmti_method_id` resolves real ids through the VM's bridge.
    rt::install_real_agent_env_bridge(&shared);
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let name = "cratonvm/test/W17MethodEvents";
    let cid = loop_class(&shared, name);
    // Widening: the class id's half of the jmethodID.
    METHOD_WATCHED.store(u64::from(cid.as_u32()) << 32, Ordering::SeqCst);
    METHOD_ENTRIES.store(0, Ordering::SeqCst);
    METHOD_EXITS.store(0, Ordering::SeqCst);
    METHOD_EXIT_VALUE.store(0, Ordering::SeqCst);
    METHOD_EXIT_POPPED.store(7, Ordering::SeqCst);

    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };

    assert_eq!(
        set_mode(env, 1, EVENT_METHOD_ENTRY, 0),
        ERR_MUST_POSSESS_CAPABILITY
    );
    let method_events: Caps = [
        CAN_GENERATE_METHOD_ENTRY_EVENTS | CAN_GENERATE_METHOD_EXIT_EVENTS,
        0,
        0,
        0,
    ];
    assert_eq!(add(env, &method_events), ERR_NONE);
    let callbacks = callbacks_with(&[
        (EVENT_METHOD_ENTRY, on_method_entry as *const () as usize),
        (EVENT_METHOD_EXIT, on_method_exit as *const () as usize),
    ]);
    // The agent's structure, as long as `jvmti.h` makes it up to
    // `MethodExit` (member 16).
    let size = JInt::try_from(17 * std::mem::size_of::<usize>()).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    let gates = &shared.debug.debugger_gates;
    assert!(!gates.jvmti_method_events_armed());
    assert_eq!(set_mode(env, 1, EVENT_METHOD_ENTRY, 0), ERR_NONE);
    assert_eq!(set_mode(env, 1, EVENT_METHOD_EXIT, 0), ERR_NONE);
    assert!(
        rt::interp_only_events_active_for_vm(vm),
        "the VM runs interpreted while they are enabled"
    );
    // Wave 27: the native-call funnel's gate follows them.
    assert!(gates.jvmti_method_events_armed() && gates.method_events_armed());
    assert!(!gates.jdwp_method_events_armed());

    run_loop(&shared, cid, name, 0);
    assert_eq!(METHOD_ENTRIES.load(Ordering::SeqCst), 1);
    assert_eq!(METHOD_EXITS.load(Ordering::SeqCst), 1);
    assert_eq!(
        METHOD_EXIT_VALUE.load(Ordering::SeqCst),
        45,
        "the jint result"
    );
    assert_eq!(METHOD_EXIT_POPPED.load(Ordering::SeqCst), 0);

    assert_eq!(set_mode(env, 0, EVENT_METHOD_ENTRY, 0), ERR_NONE);
    assert!(
        gates.jvmti_method_events_armed(),
        "MethodExit is still enabled"
    );
    run_loop(&shared, cid, name, 0);
    assert_eq!(METHOD_ENTRIES.load(Ordering::SeqCst), 1, "disabled");
    assert_eq!(METHOD_EXITS.load(Ordering::SeqCst), 2);

    assert_eq!(dispose(env), ERR_NONE);
    assert!(
        !rt::interp_only_events_active_for_vm(vm),
        "dispose takes it off"
    );
    assert!(!gates.method_events_armed(), "and the funnel's gate");
    run_loop(&shared, cid, name, 0);
    assert_eq!(METHOD_EXITS.load(Ordering::SeqCst), 2, "disposed");
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Wave 17 (proposal `interpreter-L1-proposal-jvmti-c-table-stack-and-line-functions-RETIRED-20261015`,
/// stage 1): the class and method facts through the C table —
/// `GetClassStatus`, `GetMethodModifiers`, `IsMethodNative`,
/// `GetMethodLocation` and, with `can_get_line_numbers`,
/// `GetLineNumberTable` — on a Java method with a line table and a native
/// method, with HotSpot's errors for a native method, a method without a
/// table, a bad id and a NULL out-parameter.
#[test]
fn the_c_table_answers_class_status_method_facts_and_line_numbers() {
    use cratonvm_reader::attribute::LineNumberEntry;
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let name = "cratonvm/test/W17LineAgent";
    let cid = loop_class(&shared, name);
    let bare = loop_class(&shared, "cratonvm/test/W17NoLines");
    {
        let mut cm = shared.classes.class_manager.write();
        let class = cm.get_class_mut(cid).expect("fabricated");
        if let Some(LazyAttribute::Decoded(Attribute::Code(code))) =
            class.methods[0].attributes.get_mut(0)
        {
            code.attributes.push(Attribute::LineNumberTable(vec![
                LineNumberEntry {
                    start_pc: 7,
                    line_number: 4,
                },
                LineNumberEntry {
                    start_pc: 0,
                    line_number: 3,
                },
                LineNumberEntry {
                    start_pc: 22,
                    line_number: 6,
                },
            ]));
        }
        class
            .methods
            .push(cratonvm_reader::method::ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: Arc::from("nat"),
                descriptor: Arc::from("()V"),
                attributes: vec![],
            });
    }
    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;
    let native_jmid = jmid | 1;
    let bare_jmid = u64::from(bare.as_u32()) << 32;

    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let status: extern "C" fn(*mut c_void, JObject, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 49)) };
    let modifiers: extern "C" fn(*mut c_void, u64, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 66)) };
    let lines: extern "C" fn(*mut c_void, u64, *mut JInt, *mut *mut LineNumberRow) -> JInt =
        unsafe { std::mem::transmute(function(env, 70)) };
    let location: extern "C" fn(*mut c_void, u64, *mut i64, *mut i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 71)) };
    let native: extern "C" fn(*mut c_void, u64, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 76)) };
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };

    // A fabricated class is initialized: VERIFIED | PREPARED | INITIALIZED.
    let mut s = -1;
    assert_eq!(status(env, jni::class_id_to_jclass(cid), &mut s), ERR_NONE);
    assert_eq!(s, 7);
    assert_eq!(status(env, 0, &mut s), ERR_INVALID_CLASS);
    assert_eq!(
        status(env, jni::class_id_to_jclass(cid), std::ptr::null_mut()),
        ERR_NULL_POINTER
    );

    let mut m = 0;
    assert_eq!(modifiers(env, jmid, &mut m), ERR_NONE);
    assert_eq!(m, 0x0008, "static");
    assert_eq!(modifiers(env, native_jmid, &mut m), ERR_NONE);
    assert_eq!(m, 0x0101, "public native");
    assert_eq!(modifiers(env, jmid | 7, &mut m), ERR_INVALID_METHODID);

    let mut n = 2u8;
    assert_eq!(native(env, jmid, &mut n), ERR_NONE);
    assert_eq!(n, 0);
    assert_eq!(native(env, native_jmid, &mut n), ERR_NONE);
    assert_eq!(n, 1);

    let (mut start, mut end) = (7i64, 7i64);
    assert_eq!(location(env, jmid, &mut start, &mut end), ERR_NONE);
    assert_eq!((start, end), (0, 23), "the loop's code is 24 bytes");
    assert_eq!(
        location(env, native_jmid, &mut start, &mut end),
        ERR_NATIVE_METHOD
    );

    let (mut count, mut table) = (0, std::ptr::null_mut::<LineNumberRow>());
    assert_eq!(
        lines(env, jmid, &mut count, &mut table),
        ERR_MUST_POSSESS_CAPABILITY
    );
    let line_numbers: Caps = [CAN_GET_LINE_NUMBERS, 0, 0, 0];
    assert_eq!(add(env, &line_numbers), ERR_NONE);
    assert_eq!(lines(env, jmid, &mut count, &mut table), ERR_NONE);
    assert_eq!(count, 3);
    // SAFETY: a block of `count` rows this table allocated.
    let rows = unsafe { std::slice::from_raw_parts(table, 3) }.to_vec();
    assert_eq!(
        rows,
        vec![
            LineNumberRow {
                start_location: 0,
                line_number: 3
            },
            LineNumberRow {
                start_location: 7,
                line_number: 4
            },
            LineNumberRow {
                start_location: 22,
                line_number: 6
            },
        ],
        "by start location"
    );
    assert_eq!(dealloc(env, table.cast::<u8>()), ERR_NONE);
    assert_eq!(
        lines(env, bare_jmid, &mut count, &mut table),
        ERR_ABSENT_INFORMATION
    );
    assert_eq!(
        lines(env, native_jmid, &mut count, &mut table),
        ERR_NATIVE_METHOD
    );
    assert_eq!(
        lines(env, jmid, std::ptr::null_mut(), &mut table),
        ERR_NULL_POINTER
    );

    assert_eq!(dispose(env), ERR_NONE);
    assert_eq!(modifiers(env, jmid, &mut m), ERR_INVALID_ENVIRONMENT);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

static W18_COLLECTIONS: AtomicUsize = AtomicUsize::new(0);
static W18_SECOND_CLASS: AtomicU64 = AtomicU64::new(0);

/// The first env of the wave-18 test: its callback collects, as one that
/// allocates through JNI may.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_exception_collecting(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
    _exception: JObject,
    _catch_method: u64,
    _catch_location: i64,
) {
    if let Some(vm) = jni::calling_thread_vm() {
        let mut t = JvmThread::new(ThreadId(0), "w18-collecting-agent");
        crate::runtime::interpreter::maybe_gc_forced_pub_at(&vm, &mut t, "w18-jvmti-c");
        W18_COLLECTIONS.fetch_add(1, Ordering::SeqCst);
    }
}

/// The second env: the class of the object its `exception` names.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_exception_checking(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
    exception: JObject,
    _catch_method: u64,
    _catch_location: i64,
) {
    let class = jni::jobject_to_obj(exception)
        .zip(jni::calling_thread_vm())
        .map_or(0, |(obj, vm)| {
            u64::from(vm.mem.heap.class_id_of(obj).as_u32())
        });
    W18_SECOND_CLASS.store(class, Ordering::SeqCst);
}

/// Interpreter round i1 wave 18, lane L1 (page
/// `interpreter-L2-jvmti-event-object-address-stale-for-the-next-listener-FIXED-20260925`): of
/// two C envs enabled for `Exception`, the first one's callback collects,
/// and the second one's `exception` local reference still names the
/// thrown object. It was made from the address read before the first
/// callback ran; the manager now holds the object in a global reference
/// for the delivery and hands each env its current address.
#[test]
fn a_second_c_env_gets_the_exception_after_the_first_one_collected() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    rt::install_real_agent_env_bridge(&shared);
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    W18_COLLECTIONS.store(0, Ordering::SeqCst);
    W18_SECOND_CLASS.store(0, Ordering::SeqCst);
    let name = "cratonvm/test/W18CollectingEnvs";
    let cid = loop_class(&shared, name);
    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;
    let thrown = shared.mem.heap.alloc_object(cid, 0);
    // The producer's own root (the unwinder pins the exception).
    let pin = shared.natives.jni_global_refs.lock().add(thrown);

    // SAFETY (every transmute below): the slot's `jvmti.h` signature;
    // both envs share the one table.
    let (_, probe) = get_env(0x3001_0200);
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(probe, 142)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(probe, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(probe, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(probe, 127)) };
    assert_eq!(dispose(probe), ERR_NONE);
    let mut envs = Vec::new();
    for callback in [
        on_exception_collecting as *const () as usize,
        on_exception_checking as *const () as usize,
    ] {
        let (code, env) = get_env(0x3001_0200);
        assert_eq!(code, jni::JNI_OK);
        // A solo capability goes to one env at a time (HotSpot; wave 63).
        let wanted = if envs.is_empty() {
            POTENTIAL
        } else {
            without(POTENTIAL, SOLO)
        };
        assert_eq!(add(env, &wanted), ERR_NONE);
        let callbacks = callbacks_with(&[(EVENT_EXCEPTION, callback)]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        // The first env's delivery registers first, so it runs first.
        assert_eq!(set_mode(env, 1, EVENT_EXCEPTION, 0), ERR_NONE);
        envs.push(env);
    }

    // Cast: the object's address, as the unwinder passes it
    let address = thrown.as_ptr() as usize as u64;
    rt::fire_exception_for_vm(vm, 0, jmid, 12, address, 0, -1);
    assert_eq!(
        W18_COLLECTIONS.load(Ordering::SeqCst),
        1,
        "the first env collected"
    );
    assert_eq!(
        W18_SECOND_CLASS.load(Ordering::SeqCst),
        u64::from(cid.as_u32()),
        "the second env's reference names the thrown object"
    );

    for env in envs {
        assert_eq!(dispose(env), ERR_NONE);
    }
    let _ = shared.natives.jni_global_refs.lock().remove(pin);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// The locations the `SingleStep` callback of
/// [`a_c_env_gets_single_step_for_every_bytecode`] saw.
static W22_STEPS: std::sync::Mutex<Vec<i64>> = std::sync::Mutex::new(Vec::new());

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_single_step(
    _env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    location: i64,
) {
    if let Ok(mut steps) = W22_STEPS.lock() {
        steps.push(location);
    }
}

/// Interpreter round i1 wave 22, lane L1
/// (`interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016`,
/// stage 2): `SingleStep` is delivered through the C table — its
/// capability is granted, the event (member 11 of `jvmtiEventCallbacks`)
/// reaches the agent before each bytecode of the ten-trip loop, starting
/// at 0 and ending at the `ireturn`, and a disable takes it off. Until
/// wave 22 the interpreter gated the event on a per-thread flag nothing
/// raised, so no agent received one.
#[test]
fn a_c_env_gets_single_step_for_every_bytecode() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    if let Ok(mut steps) = W22_STEPS.lock() {
        steps.clear();
    }
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    assert_eq!(
        set_mode(env, 1, EVENT_SINGLE_STEP, 0),
        ERR_MUST_POSSESS_CAPABILITY
    );
    let single_step: Caps = [CAN_GENERATE_SINGLE_STEP_EVENTS, 0, 0, 0];
    assert_eq!(add(env, &single_step), ERR_NONE);
    let callbacks =
        callbacks_with(&[(EVENT_SINGLE_STEP, on_single_step as *const () as usize)]);
    let size = JInt::try_from(13 * std::mem::size_of::<usize>()).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    let gates = &shared.debug.debugger_gates;
    assert!(!gates.jvmti_frames_armed());
    assert_eq!(set_mode(env, 1, EVENT_SINGLE_STEP, 0), ERR_NONE);
    assert!(
        rt::interp_only_events_active_for_vm(vm),
        "the VM runs interpreted while an agent steps"
    );
    // Wave 29: the native-call funnel's gate follows the step, so a
    // stood-in Java method runs its bytecode and the agent steps into it
    // (`jvmti_events::run_stood_in_java_method`); no native method event
    // is armed by it.
    assert!(gates.jvmti_frames_armed() && gates.method_events_armed());
    assert!(!gates.jvmti_method_events_armed() && !gates.jdwp_method_events_armed());

    let name = "cratonvm/test/W22StepAgent";
    let cid = loop_class(&shared, name);
    run_loop(&shared, cid, name, 0x0F22);
    let steps = W22_STEPS.lock().expect("the callback's record").clone();
    // The instruction starts of `loop_code`.
    let starts = [0, 2, 3, 4, 5, 6, 7, 8, 9, 12, 13, 14, 15, 16, 19, 22, 23];
    assert_eq!(steps.first(), Some(&0), "the first bytecode: {steps:?}");
    assert_eq!(steps.last(), Some(&23), "the ireturn: {steps:?}");
    assert!(
        steps.iter().all(|at| starts.contains(at)),
        "only instruction starts: {steps:?}"
    );
    assert!(
        steps.iter().filter(|&&at| at == 9).count() >= 11,
        "the loop test runs eleven times: {steps:?}"
    );

    assert_eq!(set_mode(env, 0, EVENT_SINGLE_STEP, 0), ERR_NONE);
    assert!(
        !gates.method_events_armed(),
        "the funnel's gate follows the disable"
    );
    run_loop(&shared, cid, name, 0x0F22);
    assert_eq!(
        W22_STEPS.lock().expect("the callback's record").len(),
        steps.len(),
        "disabled: no more steps"
    );
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// What the stack functions answered inside the first `Breakpoint`
/// callback of [`the_c_table_answers_the_current_threads_stack_in_a_callback`]:
/// `(error, value)` pairs in the order the callback asks them.
static W22_STACK: std::sync::Mutex<Vec<(JInt, i64)>> = std::sync::Mutex::new(Vec::new());

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_breakpoint_stack(
    env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    _method: u64,
    _location: i64,
) {
    let Ok(mut seen) = W22_STACK.lock() else {
        return;
    };
    if !seen.is_empty() {
        return;
    }
    let mut count: JInt = -1;
    let code = get_frame_count(env, 0, &mut count);
    seen.push((code, i64::from(count)));
    let (mut method, mut location) = (0u64, -2i64);
    let code = get_frame_location(env, 0, 0, &mut method, &mut location);
    // Cast: the jmethodID's bits, to share the vector.
    seen.push((code, method as i64));
    seen.push((code, location));
    let code = get_frame_location(env, 0, 1, &mut method, &mut location);
    seen.push((code, 0));
    let mut buffer = [FrameInfo {
        method: 0,
        location: -2,
    }; 4];
    let mut n: JInt = -1;
    let code = get_stack_trace(env, 0, 0, 4, buffer.as_mut_ptr(), &mut n);
    seen.push((code, i64::from(n)));
    seen.push((code, buffer[0].location));
    let code = get_stack_trace(env, 0, -1, 4, buffer.as_mut_ptr(), &mut n);
    seen.push((code, i64::from(n)));
    let code = get_stack_trace(env, 0, 1, 4, buffer.as_mut_ptr(), &mut n);
    seen.push((code, 0));
    let code = get_stack_trace(env, 0, -2, 4, buffer.as_mut_ptr(), &mut n);
    seen.push((code, 0));
    let code = get_stack_trace(env, 0, 0, -1, buffer.as_mut_ptr(), &mut n);
    seen.push((code, 0));
    let code = get_frame_location(env, 0, -1, &mut method, &mut location);
    seen.push((code, 0));
    let mut thread: JObject = 7;
    let code = get_current_thread(env, &mut thread);
    // Cast: the handle's bits (NULL: this thread has no mirror).
    seen.push((code, thread as i64));
}

/// Interpreter round i1 wave 22, lane L1
/// (`interpreter-L1-proposal-jvmti-c-table-stack-and-line-functions-RETIRED-20261015`, stage 2):
/// inside a `Breakpoint` callback the current thread's stack is answered
/// through the table's slots 16, 18, 19 and 104 — one frame, the loop
/// method at the breakpoint's bytecode index — with the specification's
/// errors past the bottom, for a bad start depth or count, and a thread
/// the VM does not know is `UNATTACHED_THREAD`.
#[test]
fn the_c_table_answers_the_current_threads_stack_in_a_callback() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    if let Ok(mut seen) = W22_STACK.lock() {
        seen.clear();
    }
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    assert_eq!(function(env, 16), get_frame_count as *const () as usize);
    assert_eq!(function(env, 18), get_current_thread as *const () as usize);
    assert_eq!(function(env, 19), get_frame_location as *const () as usize);
    assert_eq!(function(env, 104), get_stack_trace as *const () as usize);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let set_bp: extern "C" fn(*mut c_void, u64, i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 38)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    assert_eq!(add(env, &POTENTIAL), ERR_NONE, "binds the env to the VM");

    // An OS thread the VM does not know.
    let env_addr = env as usize; // Cast: the env's address, to cross threads
    let unattached = std::thread::spawn(move || {
        let mut count: JInt = -1;
        let env = env_addr as *mut JvmtiNativeEnv; // Cast: back to the env
        (get_frame_count(env, 0, &mut count), count)
    })
    .join()
    .expect("the probe thread");
    assert_eq!(unattached, (ERR_UNATTACHED_THREAD, -1));

    let name = "cratonvm/test/W22StackAgent";
    let cid = loop_class(&shared, name);
    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;
    let callbacks =
        callbacks_with(&[(EVENT_BREAKPOINT, on_breakpoint_stack as *const () as usize)]);
    let size = JInt::try_from(13 * std::mem::size_of::<usize>()).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    assert_eq!(set_mode(env, 1, EVENT_BREAKPOINT, 0), ERR_NONE);
    assert_eq!(set_bp(env, jmid, 9), ERR_NONE);

    // The loop runs on a registered thread whose `JvmThread` is published,
    // as every Java thread's is, so the callback's context names it.
    let registry = &shared.threads.thread_registry;
    let tid = registry.next_thread_id();
    registry.register(tid, "w22-stack", None);
    let mut thread = JvmThread::new(tid, "w22-stack");
    registry.set_jvm_thread_addr(tid, &thread as *const JvmThread as usize);
    let frame = Frame::new(
        cid,
        name.to_string(),
        "jvmtiBpLoop".to_string(),
        "()I".to_string(),
        None,
        loop_code(),
        Vec::new(),
        2,
        3,
        &[],
    );
    let r = crate::runtime::interpreter::execute_prebuilt_frame(&shared, &mut thread, frame);
    assert!(matches!(r, Ok(Some(Value::Int(45)))), "answer: {r:?}");
    registry.mark_dead(tid);

    let seen = W22_STACK.lock().expect("the callback's record").clone();
    // Cast: the jmethodID's bits, as the callback stored them.
    let jmid_bits = jmid as i64;
    assert_eq!(
        seen,
        vec![
            (ERR_NONE, 1),
            (ERR_NONE, jmid_bits),
            (ERR_NONE, 9),
            (ERR_NO_MORE_FRAMES, 0),
            (ERR_NONE, 1),
            (ERR_NONE, 9),
            (ERR_NONE, 1),
            (ERR_ILLEGAL_ARGUMENT, 0),
            (ERR_ILLEGAL_ARGUMENT, 0),
            (ERR_ILLEGAL_ARGUMENT, 0),
            (ERR_ILLEGAL_ARGUMENT, 0),
            (ERR_NONE, 0),
        ],
        "count, top location, past the bottom, the trace, the bottom one, \
         start 1 of 1, start -2 of 1, a negative count, a negative depth, \
         the current thread (no mirror)"
    );
    assert_eq!(
        stack_trace_start(0, 0),
        Some(0),
        "an empty stack from the top"
    );
    assert_eq!(stack_trace_start(0, -1), None);
    assert_eq!(stack_trace_start(3, -3), Some(0));
    assert_eq!(stack_trace_start(3, 2), Some(2));
    assert_eq!(stack_trace_start(3, 3), None);

    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Interpreter round i1 wave 42, lane L1: a running native's row goes
/// right before the position the capture gave it (wave 44, lane L3: the
/// first frame its call put on the stack), or on top; natives at one
/// position keep their order.
#[test]
fn native_rows_are_spliced_below_the_first_frame_their_call_did_not_have() {
    let entry = |name: &str| {
        crate::runtime::stackwalker::synthetic_entry(Arc::from("C"), Arc::from(name))
    };
    let names = |rows: &[FrameRow<'_>]| -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                FrameRow::Java(_, e) | FrameRow::Spliced(e) => e.method_name.to_string(),
                FrameRow::Native(_, hash) => format!("N{hash}"),
                FrameRow::Inferred(class, _) => format!("P{}", class.as_u32()),
            })
            .collect()
    };
    let c = ClassId::new(7);
    // main, a, b: interpreter frames 0..3, one row each.
    let trace = vec![entry("main"), entry("a"), entry("b")];
    let rows = splice_native_rows(&trace, &[3], &[(3, c, 1, 0)]);
    assert_eq!(
        names(&rows),
        ["main", "a", "b", "N1"],
        "called by b: on top"
    );
    let rows = splice_native_rows(&trace, &[usize::MAX], &[(3, c, 1, 0)]);
    assert_eq!(
        names(&rows),
        ["main", "a", "b", "N1"],
        "past the end: on top"
    );
    let rows = splice_native_rows(&trace, &[2], &[(2, c, 1, 0)]);
    assert_eq!(
        names(&rows),
        ["main", "a", "N1", "b"],
        "called by a, b is its upcall"
    );
    let rows = splice_native_rows(
        &trace,
        &[1, 1, 3],
        &[(1, c, 1, 0), (1, c, 2, 0), (3, c, 3, 0)],
    );
    assert_eq!(
        names(&rows),
        ["main", "N1", "N2", "a", "b", "N3"],
        "nested natives at one position keep their order"
    );
    // main, a compiled caller, a compiled method the native's own upcall
    // entered, b: the row goes between the two compiled activations.
    let trace = vec![
        entry("main"),
        entry("compiled"),
        entry("upcalled"),
        entry("b"),
    ];
    let rows = splice_native_rows(&trace, &[2], &[(1, c, 1, 1)]);
    assert_eq!(
        names(&rows),
        ["main", "compiled", "N1", "upcalled", "b"],
        "above the compiled caller, below the upcall's compiled target"
    );
    // Wave 46 (lane L1): `trace` is the four-row one here; this row
    // expected wave 42's first trace's names since wave 44 renamed it.
    let rows = splice_native_rows(&trace, &[], &[]);
    assert_eq!(
        names(&rows),
        ["main", "compiled", "upcalled", "b"],
        "no native: the trace"
    );
}

/// Wave 42: a row lives exactly as long as its guard, and nests; a JNI
/// arm's row is its function pointer, untouched (no hash on the call
/// path); a report's row for the native already innermost at this depth
/// adds none.
#[test]
fn a_native_frame_row_is_popped_with_its_guard_and_not_doubled() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(42), "w42-native-rows");
    let c = ClassId::new(9);
    let hash = native_row_method_hash("frames", "()Ljava/lang/String;");
    let ptr: *mut JvmThread = &mut thread;
    {
        // SAFETY: `thread` outlives every row of this block.
        let _jni = unsafe { NativeFrameRow::enter_jni(ptr, c, 0x1000) };
        // SAFETY: as above.
        assert_eq!(
            unsafe { (*ptr).jni_native_frames.clone() },
            vec![(0, c, 0x1000, 0)]
        );
        // SAFETY: as above.
        let _outer = unsafe { NativeFrameRow::enter_hashed(&shared, ptr, c, hash) };
        // SAFETY: as above.
        let _same = unsafe { NativeFrameRow::enter_hashed(&shared, ptr, c, hash) };
        // SAFETY: as above.
        assert_eq!(
            unsafe { (*ptr).jni_native_frames.clone() },
            vec![(0, c, 0x1000, 0), (0, c, hash | ROW_KEY_IS_HASH, 0)],
            "class 9 names no method bound to 0x1000: a row of its own; then none"
        );
        {
            // SAFETY: as above.
            let _inner = unsafe { NativeFrameRow::enter_hashed(&shared, ptr, c, hash ^ 1) };
            // SAFETY: as above.
            assert_eq!(unsafe { (*ptr).jni_native_frames.len() }, 3);
        }
        // SAFETY: as above.
        assert_eq!(unsafe { (*ptr).jni_native_frames.len() }, 2);
    }
    assert!(thread.jni_native_frames.is_empty());
    #[cfg(feature = "experimental-debug")]
    assert_eq!(
        crate::debug::jdwp_method_id("frames", "()Ljava/lang/String;"),
        hash,
        "one key for a row and a JDWP method id"
    );
}

/// Interpreter round i2 wave 60, lane L1: a lambda proxy method's row
/// names the proxy and the forwarding method's index (the functional
/// method for the SAM's own descriptor), is told apart from a native's
/// row by its key, goes right below the body's frame, and is popped with
/// its guard.
#[test]
fn a_lambda_proxy_row_names_its_method_and_sits_below_the_body() {
    use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(60), "w60-proxy-rows");
    let proxy = ClassId::new(0x8000_0060);
    let site = LambdaCallSite {
        functional_interface_id: None,
        functional_interface: Arc::from("java/lang/Runnable"),
        sam_method_name: Arc::from("run"),
        sam_descriptor: Arc::from("()V"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("p/Host"),
            member_name: Arc::from("lambda$main$0"),
            descriptor: Arc::from("()V"),
        },
        instantiated_descriptor: Arc::from("()V"),
        capture_types: Vec::new(),
        proxy_class_id: proxy,
        serializable_flag: false,
    };
    let ptr: *mut JvmThread = &mut thread;
    {
        let index = crate::runtime::lambda_proxy::lambda_proxy_forwarding_index(
            &shared, proxy, &site, "()V",
        );
        // SAFETY: `thread` outlives the row.
        let _row = unsafe { NativeFrameRow::enter_lambda_proxy(ptr, proxy, index) };
        // SAFETY: as above.
        let rows = unsafe { (*ptr).jni_native_frames.clone() };
        assert_eq!(rows, vec![(0, proxy, ROW_KEY_IS_PROXY | 1, 0)]);
        assert_eq!(proxy_row_index(rows[0].2), Some(1));
        let trace = vec![
            crate::runtime::stackwalker::synthetic_entry(
                Arc::from("p/Host"),
                Arc::from("main"),
            ),
            crate::runtime::stackwalker::synthetic_entry(
                Arc::from("p/Host"),
                Arc::from("lambda$main$0"),
            ),
        ];
        let spliced = splice_native_rows(&trace, &[1], &rows);
        assert!(
            matches!(spliced.as_slice(), [FrameRow::Java(0, _), FrameRow::Native(c, _), FrameRow::Java(1, _)] if *c == proxy),
            "between the caller and the body"
        );
    }
    assert!(thread.jni_native_frames.is_empty());
    assert_eq!(
        proxy_row_index(0x1000),
        None,
        "a JNI arm's function pointer"
    );
    assert_eq!(
        proxy_row_index(ROW_KEY_IS_HASH | ROW_KEY_IS_PROXY | 1),
        None,
        "a hashed key"
    );
}

/// The i54-L1 retirement: a lambda proxy method's frame records its
/// locals -- the proxy in slot 0, then the arguments as passed, a
/// `long`'s second slot marked, every reference pinned (a null one by the
/// proxy) -- which read back by kind, answer `INVALID_SLOT` past the
/// frame and `TYPE_MISMATCH` for another kind (MEASURED on HotSpot,
/// `L1K54JvmtiLambdaProxyLocalsAndFields`), take writes, and go with
/// their guard, pins included.
#[test]
fn a_lambda_proxy_frame_keeps_its_locals() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(61), "k54-proxy-locals");
    let proxy = ClassId::new(0x8000_0061);
    let receiver = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let arg = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let pins_before = thread.native_pin_roots.len();
    let ptr: *mut JvmThread = &mut thread;
    {
        let args = [
            Value::Int(7),
            Value::Long(9),
            Value::Object(Some(arg)),
            Value::Object(None),
        ];
        // SAFETY: `thread` outlives the frame.
        let frame = unsafe { LambdaProxyFrame::enter(ptr, proxy, 1, receiver, &args) };
        // SAFETY: as above; the frame only reads through its pointer.
        let t = unsafe { &mut *ptr };
        let key = t.jni_native_frames[0].2;
        assert_eq!(
            (proxy_row_index(key), proxy_row_locals(key)),
            (Some(1), Some(0))
        );
        let read = |t: &JvmThread, slot: JInt, kind| proxy_local_read(t, 0, slot, kind);
        assert_eq!(
            read(t, 0, LocalKind::Object),
            Ok(Value::Object(Some(receiver)))
        );
        assert_eq!(read(t, 1, LocalKind::Int), Ok(Value::Int(7)));
        assert_eq!(read(t, 2, LocalKind::Long), Ok(Value::Long(9)));
        assert_eq!(
            read(t, 3, LocalKind::Int),
            Err(ERR_TYPE_MISMATCH),
            "a long's second slot"
        );
        assert_eq!(read(t, 4, LocalKind::Object), Ok(Value::Object(Some(arg))));
        assert_eq!(read(t, 5, LocalKind::Object), Ok(Value::Object(None)));
        assert_eq!(read(t, 6, LocalKind::Object), Err(ERR_INVALID_SLOT));
        assert_eq!(read(t, -1, LocalKind::Object), Err(ERR_INVALID_SLOT));
        assert_eq!(read(t, 1, LocalKind::Object), Err(ERR_TYPE_MISMATCH));
        assert_eq!(
            proxy_local_write(t, 0, 5, LocalKind::Object, Value::Object(Some(arg))),
            Ok(())
        );
        assert_eq!(
            proxy_local_write(t, 0, 1, LocalKind::Int, Value::Int(8)),
            Ok(())
        );
        assert_eq!(
            proxy_local_write(t, 0, 2, LocalKind::Int, Value::Int(8)),
            Err(ERR_TYPE_MISMATCH)
        );
        assert_eq!(read(t, 5, LocalKind::Object), Ok(Value::Object(Some(arg))));
        assert_eq!(
            frame.current(),
            Some((
                receiver,
                vec![
                    Value::Int(8),
                    Value::Long(9),
                    Value::Object(Some(arg)),
                    Value::Object(Some(arg))
                ]
            ))
        );
        assert_eq!(
            proxy_frame_values(&t.lambda_proxy_locals, &t.native_pin_roots, key)
                .map(|v| v.len()),
            Some(6)
        );
    }
    assert!(thread.jni_native_frames.is_empty());
    assert!(thread.lambda_proxy_locals.is_empty());
    assert_eq!(thread.native_pin_roots.len(), pins_before);
}

/// Interpreter round i1 wave 44: the slot checks of the local-variable
/// functions, in HotSpot 25.0.3's order (measured,
/// `tools/probes/interp/L1/L1W44JvmtiLocalsAndStack.java`): the slot's
/// range, then whether it holds a reference, then the
/// `LocalVariableTable`.
#[test]
fn a_local_is_checked_as_hotspot_checks_it() {
    use LocalKind::{Int, Long, Object};
    let mut f = Frame::new(
        ClassId::new(0),
        "T".to_string(),
        "m".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        4,
        6,
        &[],
    );
    f.set_local(0, Value::Int(3));
    f.set_local(1, Value::Long(4));
    f.set_local(3, Value::Object(None));
    // Slots 4 and 5 are never written.
    assert_eq!(checked_local(Declared::Kind(Int), &f, 0, Int), Ok(0));
    assert_eq!(checked_local(Declared::NoTable, &f, 1, Long), Ok(1));
    assert_eq!(checked_local(Declared::Kind(Object), &f, 3, Object), Ok(3));
    assert_eq!(
        checked_local(Declared::Kind(Int), &f, 0, Long),
        Err(ERR_TYPE_MISMATCH),
        "declared another kind"
    );
    assert_eq!(
        checked_local(Declared::Kind(Int), &f, 0, Object),
        Err(ERR_TYPE_MISMATCH),
        "no reference there"
    );
    assert_eq!(
        checked_local(Declared::Kind(Object), &f, 3, Int),
        Err(ERR_TYPE_MISMATCH),
        "a reference there"
    );
    assert_eq!(
        checked_local(Declared::NotLive, &f, 3, Object),
        Err(ERR_INVALID_SLOT),
        "a reference no entry covers (javac's lock copy)"
    );
    assert_eq!(
        checked_local(Declared::NotLive, &f, 4, Object),
        Err(ERR_TYPE_MISMATCH),
        "an Object read of a variable not assigned yet"
    );
    assert_eq!(
        checked_local(Declared::NotLive, &f, 4, Int),
        Err(ERR_INVALID_SLOT),
        "an int read of a variable not assigned yet"
    );
    assert_eq!(
        checked_local(Declared::NoTable, &f, 5, Long),
        Err(ERR_INVALID_SLOT),
        "the second slot is past the locals"
    );
    assert_eq!(
        checked_local(Declared::NoTable, &f, 99, Int),
        Err(ERR_INVALID_SLOT)
    );
    assert_eq!(
        checked_local(Declared::NoTable, &f, -1, Int),
        Err(ERR_INVALID_SLOT)
    );
    assert_eq!(
        std::mem::size_of::<MonitorDepthRow>(),
        16,
        "jvmtiMonitorStackDepthInfo"
    );
}

/// Interpreter round i1 wave 52, lane L1
/// (`interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007`):
/// the phase, the env's local storage, a raw monitor's reentrant enter,
/// timed wait and owner checks, and the class, field and method facts of
/// a fabricated class through the C table. A static field's id carries
/// the static mark, so it does not name the instance field of the same
/// slot number.
#[test]
fn the_wave_52_facts_answer_through_the_c_table() {
    use cratonvm_reader::class_access_flags::FieldAccessFlags;
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let phase: extern "C" fn(*mut c_void, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 133)) };
    let set_storage: extern "C" fn(*mut c_void, usize) -> JInt =
        unsafe { std::mem::transmute(function(env, 148)) };
    let get_storage: extern "C" fn(*mut c_void, *mut usize) -> JInt =
        unsafe { std::mem::transmute(function(env, 147)) };
    let create: extern "C" fn(*mut c_void, *const u8, *mut usize) -> JInt =
        unsafe { std::mem::transmute(function(env, 31)) };
    type MonitorOp = extern "C" fn(*mut c_void, usize) -> JInt;
    let destroy: MonitorOp = unsafe { std::mem::transmute(function(env, 32)) };
    let enter: MonitorOp = unsafe { std::mem::transmute(function(env, 33)) };
    let exit: MonitorOp = unsafe { std::mem::transmute(function(env, 34)) };
    let notify: MonitorOp = unsafe { std::mem::transmute(function(env, 36)) };
    let notify_all: MonitorOp = unsafe { std::mem::transmute(function(env, 37)) };
    let wait: extern "C" fn(*mut c_void, usize, i64) -> JInt =
        unsafe { std::mem::transmute(function(env, 35)) };
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let modifiers: extern "C" fn(*mut c_void, JObject, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 51)) };
    type ClassBool = extern "C" fn(*mut c_void, JObject, *mut u8) -> JInt;
    let is_interface: ClassBool = unsafe { std::mem::transmute(function(env, 55)) };
    let is_array: ClassBool = unsafe { std::mem::transmute(function(env, 56)) };
    let source: extern "C" fn(*mut c_void, JObject, *mut *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 50)) };
    let fields: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut *mut u64) -> JInt =
        unsafe { std::mem::transmute(function(env, 53)) };
    let field_name: extern "C" fn(
        *mut c_void,
        JObject,
        u64,
        *mut *mut u8,
        *mut *mut u8,
        *mut *mut u8,
    ) -> JInt = unsafe { std::mem::transmute(function(env, 60)) };
    let field_mods: extern "C" fn(*mut c_void, JObject, u64, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 62)) };
    let field_synthetic: extern "C" fn(*mut c_void, JObject, u64, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 63)) };
    type MethodJint = extern "C" fn(*mut c_void, u64, *mut JInt) -> JInt;
    let max_locals: MethodJint = unsafe { std::mem::transmute(function(env, 68)) };
    let args_size: MethodJint = unsafe { std::mem::transmute(function(env, 69)) };
    let version: extern "C" fn(*mut c_void, JObject, *mut JInt, *mut JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 145)) };
    let dealloc: extern "C" fn(*mut c_void, *mut u8) -> JInt =
        unsafe { std::mem::transmute(function(env, 47)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let text = |block: *mut u8| -> Option<String> {
        if block.is_null() {
            return None;
        }
        // SAFETY: a NUL-terminated block the table allocated.
        let s = unsafe { std::ffi::CStr::from_ptr(block.cast()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(dealloc(env, block), ERR_NONE);
        Some(s)
    };

    let mut p = 0;
    assert_eq!(phase(env, &mut p), ERR_NONE);
    assert_eq!(p, JInt::from(crate::debug::JVMTI_PHASE_LIVE));
    let mut storage = 1usize;
    assert_eq!(get_storage(env, &mut storage), ERR_NONE);
    assert_eq!(storage, 0, "nothing stored yet");
    assert_eq!(set_storage(env, 0x5200), ERR_NONE);
    assert_eq!(get_storage(env, &mut storage), ERR_NONE);
    assert_eq!(storage, 0x5200);

    let mut monitor = 0usize;
    assert_eq!(create(env, c"w52".as_ptr().cast(), &mut monitor), ERR_NONE);
    assert_eq!(exit(env, monitor), ERR_NOT_MONITOR_OWNER);
    assert_eq!(notify(env, monitor), ERR_NOT_MONITOR_OWNER);
    assert_eq!(wait(env, monitor, 1), ERR_NOT_MONITOR_OWNER);
    assert_eq!(enter(env, monitor), ERR_NONE);
    assert_eq!(enter(env, monitor), ERR_NONE, "reentrant");
    assert_eq!(wait(env, monitor, 1), ERR_NONE, "timed out, owned again");
    assert_eq!(notify(env, monitor), ERR_NONE);
    assert_eq!(notify_all(env, monitor), ERR_NONE);
    assert_eq!(exit(env, monitor), ERR_NONE);
    assert_eq!(exit(env, monitor), ERR_NONE, "the wait kept both entries");
    assert_eq!(exit(env, monitor), ERR_NOT_MONITOR_OWNER);
    // Another thread takes it once this one lets it go.
    assert_eq!(enter(env, monitor), ERR_NONE);
    let env_addr = env as usize; // Cast: the env's address, for the other thread
    let other = std::thread::spawn(move || {
        let env = env_addr as *mut c_void; // Cast: the address back
        (enter(env, monitor), exit(env, monitor))
    });
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(exit(env, monitor), ERR_NONE);
    assert_eq!(other.join().expect("joined"), (ERR_NONE, ERR_NONE));
    assert_eq!(destroy(env, monitor), ERR_NONE);
    assert_eq!(enter(env, 0), ERR_INVALID_MONITOR);

    let cid = loop_class(&shared, "cratonvm/test/W52Facts");
    {
        let mut cm = shared.classes.class_manager.write();
        let class = cm.get_class_mut(cid).expect("fabricated");
        let field = |flags: FieldAccessFlags, name: &str, descriptor: &str| {
            cratonvm_reader::field::ClassFileField {
                access_flags: flags,
                name: Arc::from(name),
                descriptor: Arc::from(descriptor),
                attributes: Vec::new(),
            }
        };
        class.fields = vec![
            field(
                FieldAccessFlags::PUBLIC | FieldAccessFlags::STATIC | FieldAccessFlags::FINAL,
                "LABEL",
                "Ljava/lang/String;",
            ),
            field(FieldAccessFlags::PRIVATE, "count", "I"),
            field(FieldAccessFlags::STATIC, "created", "I"),
        ];
        class.fields[1]
            .attributes
            .push(LazyAttribute::Decoded(Attribute::Synthetic));
        class.first_field_index = 0;
        class.source_file = Some("W52Facts.java".to_string());
        class.state = crate::classloading::ClassState::Loaded;
    }
    let klass = jni::class_id_to_jclass(cid);
    let mut answer: JInt = -1;
    assert_eq!(modifiers(env, klass, &mut answer), ERR_NONE);
    let mut flag = 2u8;
    assert_eq!(is_interface(env, klass, &mut flag), ERR_NONE);
    assert_eq!(flag, 0, "not an interface");
    assert_eq!(is_array(env, klass, &mut flag), ERR_NONE);
    assert_eq!(flag, 0, "not an array");
    assert_eq!(is_array(env, 0, &mut flag), ERR_INVALID_CLASS);
    let mut src = std::ptr::null_mut::<u8>();
    assert_eq!(
        source(env, klass, &mut src),
        ERR_MUST_POSSESS_CAPABILITY,
        "can_get_source_file_name first"
    );
    let wanted: Caps = [
        CAN_GET_SOURCE_FILE_NAME | CAN_GET_SYNTHETIC_ATTRIBUTE,
        0,
        0,
        0,
    ];
    assert_eq!(add(env, &wanted), ERR_NONE);
    assert_eq!(source(env, klass, &mut src), ERR_NONE);
    assert_eq!(text(src).as_deref(), Some("W52Facts.java"));
    let (mut minor, mut major) = (-1, -1);
    assert_eq!(version(env, klass, &mut minor, &mut major), ERR_NONE);

    let mut count: JInt = -1;
    let mut ids = std::ptr::null_mut::<u64>();
    assert_eq!(
        fields(env, klass, &mut count, &mut ids),
        ERR_CLASS_NOT_PREPARED,
        "loaded, not linked: HotSpot's answer"
    );
    shared
        .classes
        .class_manager
        .write()
        .get_class_mut(cid)
        .expect("fabricated")
        .state = crate::classloading::ClassState::Prepared;
    assert_eq!(fields(env, klass, &mut count, &mut ids), ERR_NONE);
    assert_eq!(count, 3);
    // SAFETY: the three ids the table allocated.
    let list = unsafe { std::slice::from_raw_parts(ids, 3) }.to_vec();
    assert_eq!(dealloc(env, ids.cast()), ERR_NONE);
    let class_half = u64::from(cid.as_u32()) << 32;
    assert_eq!(
        list,
        vec![
            STATIC_FIELD_ID | class_half,
            class_half,
            STATIC_FIELD_ID | class_half | 1
        ],
        "class-file order; the statics marked"
    );
    let named: Vec<(Option<String>, Option<String>, JInt, u8)> = list
        .iter()
        .map(|&id| {
            let (mut n, mut s) = (std::ptr::null_mut(), std::ptr::null_mut());
            assert_eq!(
                field_name(env, klass, id, &mut n, &mut s, std::ptr::null_mut()),
                ERR_NONE
            );
            let mut mods = -1;
            assert_eq!(field_mods(env, klass, id, &mut mods), ERR_NONE);
            let mut synthetic = 2u8;
            assert_eq!(field_synthetic(env, klass, id, &mut synthetic), ERR_NONE);
            (text(n), text(s), mods, synthetic)
        })
        .collect();
    assert_eq!(
        named,
        vec![
            (
                Some("LABEL".into()),
                Some("Ljava/lang/String;".into()),
                0x19,
                0
            ),
            (Some("count".into()), Some("I".into()), 0x2, 1),
            (Some("created".into()), Some("I".into()), 0x8, 0),
        ]
    );
    let mut n = std::ptr::null_mut();
    assert_eq!(
        field_name(
            env,
            klass,
            class_half | 7,
            &mut n,
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ),
        ERR_INVALID_FIELDID
    );

    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;
    let mut slots = -1;
    assert_eq!(max_locals(env, jmid, &mut slots), ERR_NONE);
    assert_eq!(slots, 3, "max_locals");
    assert_eq!(args_size(env, jmid, &mut slots), ERR_NONE);
    assert_eq!(slots, 0, "a static ()I takes no argument slot");

    assert_eq!(dispose(env), ERR_NONE);
    assert_eq!(phase(env, &mut p), ERR_INVALID_ENVIRONMENT);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// The `'static` tally a test leaks and hands its callback through the
/// env's local storage (slot 148), so the tests declare no static (the
/// per-VM statics ratchet counts test modules too).
fn leak_into_env_storage<T>(env: *mut c_void, value: T) -> &'static T {
    let leaked: &'static T = Box::leak(Box::new(value));
    assert_eq!(
        set_environment_local_storage(env.cast(), std::ptr::from_ref(leaked) as usize),
        ERR_NONE
    );
    leaked
}

/// What [`leak_into_env_storage`] stored; `None` before any.
fn env_storage<T>(env: *mut JvmtiNativeEnv) -> Option<&'static T> {
    let mut data = 0usize;
    if get_environment_local_storage(env, &mut data) != ERR_NONE || data == 0 {
        return None;
    }
    // SAFETY: a leaked `T` the test stored with `leak_into_env_storage`.
    Some(unsafe { &*(data as *const T) })
}

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_thread_event(env: *mut JvmtiNativeEnv, _jni: JNIEnv, _thread: JObject) {
    if let Some(events) = env_storage::<AtomicUsize>(env) {
        events.fetch_add(1, Ordering::SeqCst);
    }
}

/// Interpreter round i1 wave 52, lane L1: `ThreadStart` / `ThreadEnd`
/// through the C table. Enabling either raises the VM's gate
/// (`DebuggerGates::native_thread_events_armed`), the thread's post
/// reaches the agent's member 2 / 3, and disabling both lowers the gate.
#[test]
fn a_c_env_gets_thread_start_and_end() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let events = leak_into_env_storage(env, AtomicUsize::new(0));
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let callback = on_thread_event as *const () as usize;
    let callbacks =
        callbacks_with(&[(EVENT_THREAD_START, callback), (EVENT_THREAD_END, callback)]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    let gates = &shared.debug.debugger_gates;
    assert!(!gates.native_thread_events_armed());
    post_thread_event(&shared, 7, true);
    assert_eq!(events.load(Ordering::SeqCst), 0, "not enabled");
    assert_eq!(set_mode(env, 1, EVENT_THREAD_START, 0), ERR_NONE);
    assert!(gates.native_thread_events_armed());
    post_thread_event(&shared, 7, true);
    post_thread_event(&shared, 7, false);
    assert_eq!(events.load(Ordering::SeqCst), 1, "ThreadEnd is not enabled");
    assert_eq!(set_mode(env, 1, EVENT_THREAD_END, 0), ERR_NONE);
    post_thread_event(&shared, 7, false);
    assert_eq!(events.load(Ordering::SeqCst), 2);
    assert_eq!(set_mode(env, 0, EVENT_THREAD_START, 0), ERR_NONE);
    assert!(
        gates.native_thread_events_armed(),
        "ThreadEnd still listens"
    );
    assert_eq!(set_mode(env, 0, EVENT_THREAD_END, 0), ERR_NONE);
    assert!(!gates.native_thread_events_armed());
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

// gci i12/ev: the JNI binding the callback runs under (0: none). No
// assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_thread_event_binding(
    env: *mut JvmtiNativeEnv,
    _jni: JNIEnv,
    _thread: JObject,
) {
    if let Some(seen) = env_storage::<AtomicUsize>(env) {
        seen.store(
            jni::installed_jni_thread_addr().unwrap_or(0),
            Ordering::SeqCst,
        );
    }
}

/// gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
/// item 1c): a `ThreadStart` / `ThreadEnd` posted the way the thread
/// start path now posts it (under `post_on_own_thread`) reaches the
/// agent's callback with the poster's own handle installed, and the
/// binding is put back afterwards; posted bare, the callback runs with no
/// handle of an unpublished thread (the registry has none for it).
#[test]
fn gci_i12ev_a_thread_event_callback_runs_on_the_posters_handle() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let seen = leak_into_env_storage(env, AtomicUsize::new(usize::MAX));
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let callback = on_thread_event_binding as *const () as usize;
    let callbacks =
        callbacks_with(&[(EVENT_THREAD_START, callback), (EVENT_THREAD_END, callback)]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    assert_eq!(set_mode(env, 1, EVENT_THREAD_START, 0), ERR_NONE);
    assert_eq!(set_mode(env, 1, EVENT_THREAD_END, 0), ERR_NONE);
    let before = jni::installed_jni_thread_addr();
    let mut poster = JvmThread::new(ThreadId(7), "gci-i12ev-poster");
    let own = &mut poster as *mut JvmThread as usize;
    for start in [true, false] {
        seen.store(usize::MAX, Ordering::SeqCst);
        crate::runtime::interpreter::post_on_own_thread(&mut poster, || {
            post_thread_event(&shared, 7, start)
        });
        assert_eq!(
            seen.load(Ordering::SeqCst),
            own,
            "start={start}: the poster's handle"
        );
        assert_eq!(
            jni::installed_jni_thread_addr(),
            before,
            "start={start}: put back"
        );
    }
    if before.is_none() {
        seen.store(usize::MAX, Ordering::SeqCst);
        post_thread_event(&shared, 7, true);
        assert_eq!(seen.load(Ordering::SeqCst), 0, "bare: no handle installed");
    }
    assert_eq!(set_mode(env, 0, EVENT_THREAD_START, 0), ERR_NONE);
    assert_eq!(set_mode(env, 0, EVENT_THREAD_END, 0), ERR_NONE);
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// The `(method, location, class id)` rows `on_exception_catch` saw.
type Catches = std::sync::Mutex<Vec<(u64, i64, u64)>>;

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_exception_catch(
    env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _thread: JObject,
    method: u64,
    location: i64,
    exception: JObject,
) {
    let class = jni::jobject_to_obj(exception)
        .zip(jni::calling_thread_vm())
        .map_or(0, |(obj, vm)| {
            u64::from(vm.mem.heap.class_id_of(obj).as_u32())
        });
    if let Some(Ok(mut seen)) = env_storage::<Catches>(env).map(std::sync::Mutex::lock) {
        seen.push((method, location, class));
    }
}

/// Interpreter round i1 wave 52, lane L1 (the last item of
/// `interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016`):
/// `ExceptionCatch` through the C table — its capability is
/// `can_generate_exception_events`, the event (member 9 of
/// `jvmtiEventCallbacks`) reaches the agent with the handler's method and
/// location and the caught object as a local reference, and a disable
/// takes it off the VM's manager. With `CRATONVM_JVMTI_C_EXCEPTION_CATCH`
/// off an enable answers `JVMTI_ERROR_NOT_AVAILABLE`, as before.
#[test]
fn a_c_env_gets_exception_catch_with_the_caught_object() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let cid = loop_class(&shared, "cratonvm/test/W52Catch");
    // Widening: the class id's half of the jmethodID.
    let jmid = u64::from(cid.as_u32()) << 32;
    let caught = shared.mem.heap.alloc_object(cid, 0);
    let pin = shared.natives.jni_global_refs.lock().add(caught);
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let catches = leak_into_env_storage(env, Catches::default());
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let add: extern "C" fn(*mut c_void, *const Caps) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let callbacks = callbacks_with(&[(
        EVENT_EXCEPTION_CATCH,
        on_exception_catch as *const () as usize,
    )]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    if !shared.debug.debugger_gates.c_exception_catch() {
        assert_eq!(add(env, &POTENTIAL), ERR_NONE);
        assert_eq!(
            set_mode(env, 1, EVENT_EXCEPTION_CATCH, 0),
            ERR_NOT_AVAILABLE
        );
        assert_eq!(dispose(env), ERR_NONE);
        let _ = shared.natives.jni_global_refs.lock().remove(pin);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
        return;
    }
    assert_eq!(
        set_mode(env, 1, EVENT_EXCEPTION_CATCH, 0),
        ERR_MUST_POSSESS_CAPABILITY
    );
    let exceptions: Caps = [CAN_GENERATE_EXCEPTION_EVENTS, 0, 0, 0];
    assert_eq!(add(env, &exceptions), ERR_NONE);
    assert!(!rt::exception_catch_enabled_for_vm(vm, 5));
    assert_eq!(set_mode(env, 1, EVENT_EXCEPTION_CATCH, 0), ERR_NONE);
    assert!(
        rt::exception_catch_enabled_for_vm(vm, 5),
        "the delivery env raised the manager's ExceptionCatch"
    );

    // Cast: the object's address, as the catch hook passes it
    let address = caught.as_ptr() as usize as u64;
    rt::fire_exception_catch_for_vm(vm, 5, jmid, 22, address);
    assert_eq!(
        *catches.lock().expect("catches"),
        vec![(jmid, 22, u64::from(cid.as_u32()))],
        "the handler's method and location, and the caught object"
    );

    assert_eq!(set_mode(env, 0, EVENT_EXCEPTION_CATCH, 0), ERR_NONE);
    assert!(
        !rt::exception_catch_enabled_for_vm(vm, 5),
        "a disable reaches the VM"
    );
    rt::fire_exception_catch_for_vm(vm, 5, jmid, 22, address);
    assert_eq!(catches.lock().expect("catches").len(), 1);
    assert_eq!(dispose(env), ERR_NONE);
    let _ = shared.natives.jni_global_refs.lock().remove(pin);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// The `(name, class data length, loader is NULL)` rows
/// `on_class_file_load_hook` saw.
type Hooked = std::sync::Mutex<Vec<(String, usize, bool)>>;

// No assertion in here: a panic cannot leave an `extern "C"` function.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn on_class_file_load_hook(
    env: *mut JvmtiNativeEnv,
    _jni_env: JNIEnv,
    _class_being_redefined: JObject,
    loader: JObject,
    name: *const u8,
    _domain: JObject,
    len: JInt,
    data: *const u8,
    new_len: *mut JInt,
    new_data: *mut *mut u8,
) {
    let name = std::ffi::CStr::from_ptr(name.cast())
        .to_string_lossy()
        .into_owned();
    let Ok(size) = usize::try_from(len) else {
        return;
    };
    if name == "w53/Patched" {
        let mut copy = std::ptr::null_mut::<u8>();
        if allocate(env, i64::from(len), &mut copy) == ERR_NONE && !copy.is_null() {
            std::ptr::copy_nonoverlapping(data, copy, size);
            *copy.add(size - 1) = b'!';
            *new_len = len;
            *new_data = copy;
        }
    }
    if let Some(Ok(mut seen)) = env_storage::<Hooked>(env).map(std::sync::Mutex::lock) {
        seen.push((name, size, loader == 0));
    }
}

/// Interpreter round i2 wave 53, lane L1: `ClassFileLoadHook` through the
/// C table. Enabling it (no capability) raises the VM's gate
/// (`DebuggerGates::native_class_file_load_hook_armed`), which the
/// transformer chain's walk asks through `class_file_load_hook_vm`; the
/// callback (member 4) gets the name and the class file, and its
/// replacement, an `Allocate` block, is the answer. A class it declines
/// answers `None`; a disable lowers the gate. With
/// `CRATONVM_JVMTI_C_CLASS_HOOKS` off an enable answers
/// `JVMTI_ERROR_NOT_AVAILABLE`, as before.
#[test]
fn a_c_class_file_load_hook_rewrites_the_class_file() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let hooked = leak_into_env_storage(env, Hooked::default());
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let callbacks = callbacks_with(&[(
        EVENT_CLASS_FILE_LOAD_HOOK,
        on_class_file_load_hook as *const () as usize,
    )]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    let gates = &shared.debug.debugger_gates;
    if !gates.c_class_hooks() {
        assert_eq!(
            set_mode(env, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0),
            ERR_NOT_AVAILABLE
        );
        assert!(!gates.native_class_file_load_hook_armed());
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
        return;
    }
    assert!(class_file_load_hook_vm(vm).is_none(), "not enabled");
    assert_eq!(set_mode(env, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
    assert!(gates.native_class_file_load_hook_armed());
    assert!(class_file_load_hook_vm(vm).is_some());

    let bytes = b"\xCA\xFE\xBA\xBE-w53";
    let rewritten =
        post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, false);
    assert_eq!(rewritten.as_deref(), Some(&b"\xCA\xFE\xBA\xBE-w5!"[..]));
    let declined = post_class_file_load_hook(&shared, 0, "w53/Other", [None; 3], bytes, false);
    assert_eq!(declined, None, "a declined class file is not replaced");
    // Wave 54: an env without `can_retransform_classes` is not in the
    // retransform-capable group.
    let other_group =
        post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, true);
    assert_eq!(other_group, None);
    assert_eq!(
        *hooked.lock().expect("hooked"),
        vec![
            ("w53/Patched".to_string(), bytes.len(), true),
            ("w53/Other".to_string(), bytes.len(), true),
        ]
    );

    assert_eq!(set_mode(env, 0, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
    assert!(!gates.native_class_file_load_hook_armed());
    assert!(class_file_load_hook_vm(vm).is_none());
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// Interpreter round i2 wave 54, lane L1: `can_retransform_classes` and
/// `RetransformClasses` (152). While `CRATONVM_JVMTI_C_RETRANSFORM` is on
/// the capability is potential, an env holding it is in the
/// retransform-capable group of `ClassFileLoadHook` (and only there), and
/// `RetransformClasses` checks its arguments in HotSpot's order; off, the
/// capability is not potential and the call answers
/// `JVMTI_ERROR_MUST_POSSESS_CAPABILITY`.
#[test]
fn a_c_env_with_can_retransform_classes_takes_the_retransform_group() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let cid = loop_class(&shared, "cratonvm/test/W54Retransformed");
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let hooked = leak_into_env_storage(env, Hooked::default());
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let potential: extern "C" fn(*mut c_void, *mut [u32; CAPABILITY_WORDS]) -> JInt =
        unsafe { std::mem::transmute(function(env, 140)) };
    let add: extern "C" fn(*mut c_void, *const [u32; CAPABILITY_WORDS]) -> JInt =
        unsafe { std::mem::transmute(function(env, 142)) };
    let retransform: extern "C" fn(*mut c_void, JInt, *const JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 152)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let klass = jni::class_id_to_jclass(cid);
    assert_eq!(
        retransform(env, 1, &klass),
        ERR_MUST_POSSESS_CAPABILITY,
        "not held yet"
    );
    let mut caps = [0u32; CAPABILITY_WORDS];
    assert_eq!(potential(env, &mut caps), ERR_NONE);
    let wanted = [0, CAN_RETRANSFORM_CLASSES, 0, 0];
    let gates = &shared.debug.debugger_gates;
    if !gates.c_retransform() {
        assert_eq!(caps[1] & CAN_RETRANSFORM_CLASSES, 0);
        assert_eq!(add(env, &wanted), ERR_NOT_AVAILABLE);
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
        return;
    }
    assert_ne!(
        caps[1] & CAN_RETRANSFORM_CLASSES,
        0,
        "potential while the switch is on"
    );
    assert_eq!(add(env, &wanted), ERR_NONE);
    assert_eq!(retransform(env, -1, &klass), ERR_ILLEGAL_ARGUMENT);
    assert_eq!(retransform(env, 1, std::ptr::null()), ERR_NULL_POINTER);
    let bogus: JObject = 0;
    assert_eq!(retransform(env, 1, &bogus), ERR_INVALID_CLASS);
    if gates.c_class_hooks() {
        let callbacks = callbacks_with(&[(
            EVENT_CLASS_FILE_LOAD_HOOK,
            on_class_file_load_hook as *const () as usize,
        )]);
        let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
        assert_eq!(
            set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
            ERR_NONE
        );
        assert_eq!(set_mode(env, 1, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
        let bytes = b"\xCA\xFE\xBA\xBE-w54";
        let first =
            post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, false);
        assert_eq!(first, None, "not in the non-retransformable group");
        let second =
            post_class_file_load_hook(&shared, 0, "w53/Patched", [None; 3], bytes, true);
        assert_eq!(second.as_deref(), Some(&b"\xCA\xFE\xBA\xBE-w5!"[..]));
        assert_eq!(hooked.lock().expect("hooked").len(), 1, "offered once");
        assert_eq!(set_mode(env, 0, EVENT_CLASS_FILE_LOAD_HOOK, 0), ERR_NONE);
    }
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

// No assertion in here: a panic cannot leave an `extern "C"` function.
unsafe extern "C" fn on_class_load(
    env: *mut JvmtiNativeEnv,
    _jni: JNIEnv,
    _thread: JObject,
    klass: JObject,
) {
    if let Some(loaded) = env_storage::<AtomicU64>(env) {
        let id =
            jni::jclass_class_id_exact(klass).map_or(u64::MAX, |id| u64::from(id.as_u32()));
        loaded.store(id, Ordering::SeqCst);
    }
}

/// Interpreter round i2 wave 53, lane L1: `ClassLoad` through the C table
/// (member 5, `ClassPrepare`'s signature), behind its own gate
/// (`DebuggerGates::native_class_load_armed`).
#[test]
fn a_c_env_gets_class_load() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let cid = loop_class(&shared, "cratonvm/test/W53Loaded");
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let loaded = leak_into_env_storage(env, AtomicU64::new(0));
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let callbacks = callbacks_with(&[(EVENT_CLASS_LOAD, on_class_load as *const () as usize)]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    let gates = &shared.debug.debugger_gates;
    if !gates.c_class_hooks() {
        assert_eq!(set_mode(env, 1, EVENT_CLASS_LOAD, 0), ERR_NOT_AVAILABLE);
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
        return;
    }
    post_class_load(&shared, 0, cid);
    assert_eq!(loaded.load(Ordering::SeqCst), 0, "not enabled");
    assert_eq!(set_mode(env, 1, EVENT_CLASS_LOAD, 0), ERR_NONE);
    assert!(gates.native_class_load_armed());
    post_class_load(&shared, 0, cid);
    assert_eq!(loaded.load(Ordering::SeqCst), u64::from(cid.as_u32()));
    assert_eq!(set_mode(env, 0, EVENT_CLASS_LOAD, 0), ERR_NONE);
    assert!(!gates.native_class_load_armed());
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}

/// gci i12/ev (`CRATONVM_JVMTI_DEFER_UNOWNED_CLASS_LOAD`): the decisions.
/// A `ClassLoad` waits only on a published thread whose own handle is not
/// installed, and only with the switch on (asked last); the queue hands
/// each thread its own entries in order.
#[test]
fn gci_i12ev_a_class_load_waits_only_without_the_threads_own_handle() {
    let asked = std::cell::Cell::new(0);
    let on = || {
        asked.set(asked.get() + 1);
        true
    };
    assert!(own_handle_installed(Some(0x1000), Some(0x1000)));
    assert!(!own_handle_installed(Some(0x1000), Some(0x2000)));
    assert!(!own_handle_installed(Some(0x1000), None));
    assert!(!own_handle_installed(None, None));
    assert!(
        !class_load_deferred(Some(0x1000), Some(0x1000), on),
        "own handle: now"
    );
    assert!(
        !class_load_deferred(None, None, on),
        "unpublished thread: now"
    );
    assert_eq!(asked.get(), 0, "the switch is asked last");
    assert!(class_load_deferred(Some(0x1000), None, on));
    assert!(class_load_deferred(Some(0x1000), Some(0x2000), on));
    assert!(
        !class_load_deferred(Some(0x1000), None, || false),
        "switch off: now"
    );
    let (a, b, c) = (ClassId::new(1), ClassId::new(2), ClassId::new(3));
    let mut queue = vec![(7, a), (8, b), (7, c)];
    assert_eq!(take_deferred_class_loads(&mut queue, 7), vec![a, c]);
    assert_eq!(queue, vec![(8, b)]);
    assert!(take_deferred_class_loads(&mut queue, 9).is_empty());
    assert_eq!(take_deferred_class_loads(&mut queue, 8), vec![b]);
    assert!(queue.is_empty());
}

/// gci i12/ev: a deferred `ClassLoad` reaches the agent at its thread's
/// flush, run under that thread's own handle; another thread's entry
/// waits, and the gate drops with the queue empty.
#[test]
fn gci_i12ev_a_deferred_class_load_is_delivered_at_its_threads_flush() {
    let _lock = rt::jvmti_registry_test_lock();
    let shared = vm_fixture();
    let vm = shared.vm_identity;
    jni::set_jni_context_arc(Arc::clone(&shared));
    let cid = loop_class(&shared, "cratonvm/test/I12EvDeferred");
    let (code, env) = get_env(0x3001_0200);
    assert_eq!(code, jni::JNI_OK);
    let loaded = leak_into_env_storage(env, AtomicU64::new(0));
    // SAFETY (every transmute below): the slot's `jvmti.h` signature.
    let set_mode: extern "C" fn(*mut c_void, JInt, JInt, JObject) -> JInt =
        unsafe { std::mem::transmute(function(env, 2)) };
    let set_callbacks: extern "C" fn(*mut c_void, *const u8, JInt) -> JInt =
        unsafe { std::mem::transmute(function(env, 122)) };
    let dispose: extern "C" fn(*mut c_void) -> JInt =
        unsafe { std::mem::transmute(function(env, 127)) };
    let callbacks = callbacks_with(&[(EVENT_CLASS_LOAD, on_class_load as *const () as usize)]);
    let size = JInt::try_from(std::mem::size_of_val(&callbacks)).expect("small");
    assert_eq!(
        set_callbacks(env, callbacks.as_ptr().cast::<u8>(), size),
        ERR_NONE
    );
    if !shared.debug.debugger_gates.c_class_hooks() {
        assert_eq!(dispose(env), ERR_NONE);
        rt::forget_vm_jvmti_state(vm);
        jni::clear_jni_context();
        return;
    }
    assert_eq!(set_mode(env, 1, EVENT_CLASS_LOAD, 0), ERR_NONE);
    let pending = &shared.debug.deferred_class_loads_pending;
    // As `post_class_load` defers them: queued under the lock, gate up.
    {
        let mut state = shared.debug.jvmti_env.lock();
        state.deferred_class_loads.push((7, cid));
        state.deferred_class_loads.push((8, cid));
        pending.store(true, Ordering::Release);
    }
    let mut seven = JvmThread::new(ThreadId(7), "gci-i12ev-seven");
    crate::runtime::interpreter::post_on_own_thread(&mut seven, || {
        flush_deferred_class_loads(&shared, 7)
    });
    assert_eq!(
        loaded.load(Ordering::SeqCst),
        u64::from(cid.as_u32()),
        "thread 7's load"
    );
    assert_eq!(
        shared.debug.jvmti_env.lock().deferred_class_loads,
        vec![(8, cid)]
    );
    assert!(
        pending.load(Ordering::Acquire),
        "thread 8's entry still waits"
    );
    loaded.store(0, Ordering::SeqCst);
    let mut eight = JvmThread::new(ThreadId(8), "gci-i12ev-eight");
    crate::runtime::interpreter::post_on_own_thread(&mut eight, || {
        flush_deferred_class_loads(&shared, 8)
    });
    assert_eq!(
        loaded.load(Ordering::SeqCst),
        u64::from(cid.as_u32()),
        "thread 8's load"
    );
    assert!(shared
        .debug
        .jvmti_env
        .lock()
        .deferred_class_loads
        .is_empty());
    assert!(
        !pending.load(Ordering::Acquire),
        "the gate drops with the queue empty"
    );
    // Nothing waits: a flush is one load.
    loaded.store(0, Ordering::SeqCst);
    flush_deferred_class_loads(&shared, 7);
    assert_eq!(loaded.load(Ordering::SeqCst), 0);
    assert_eq!(set_mode(env, 0, EVENT_CLASS_LOAD, 0), ERR_NONE);
    assert_eq!(dispose(env), ERR_NONE);
    rt::forget_vm_jvmti_state(vm);
    jni::clear_jni_context();
}
