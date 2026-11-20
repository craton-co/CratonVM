use super::{
    init_static_native_owner, invoke_static_or_native, receiver_is_class_loader,
    static_by_name_init_enforced, static_owner_init_pending, SharedVm,
};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::error::MethodCallResult;
use crate::native::registry::{NativeCallback, NativeContext};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

type NativeFn = NativeCallback;

fn config(jdk_only: bool) -> crate::config::VmConfig {
    let mut config = crate::config::VmConfig::default();
    if jdk_only {
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        // `JdkOnly` with the synthetic library is a rejected pair.
        config.use_synthetic_jdk = false;
    }
    config
}

/// A VM with `owner.m()V` and `owner.<clinit>()V` registered as `Bridge`
/// natives, and `owner` defined as a LOADED class (not initialized) that
/// declares both as `static native`.
fn vm_with_owner(
    jdk_only: bool,
    owner: &str,
    clinit: NativeFn,
    m: NativeFn,
) -> (Arc<SharedVm>, ClassId) {
    let mut vm = SharedVm::new(config(jdk_only));
    for (name, callback) in [("<clinit>", clinit), ("m", m)] {
        vm.natives.native_methods.register(
            owner,
            name,
            "()V",
            callback,
            cratonvm_native_api::NativeKind::Bridge,
        );
    }
    let shared = Arc::new(vm);
    let native = |name: &str, flags: MethodAccessFlags| ClassFileMethod {
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        access_flags: flags | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
        attributes: vec![],
    };
    let id = {
        let mut cm = shared.classes.class_manager.write();
        let id = cm.class_store.next_id();
        cm.class_store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: cratonvm_types::intern_arc(owner),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![
                native("m", MethodAccessFlags::PUBLIC),
                native("<clinit>", MethodAccessFlags::empty()),
            ],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info: None,
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        });
        cm.register_class_name(ClassLoaderId::Application, owner, id);
        id
    };
    assert!(!crate::vm::is_class_initialized_via_manager(&shared, id));
    (shared, id)
}

/// Under `--real-jdk` the static entry runs `<clinit>` before the
/// registered static native, once: the native never observes its class
/// uninitialized, and the second call does not re-run the initializer.
#[test]
fn the_static_entry_initializes_before_a_registered_static_native_under_jdk_only() {
    static CLINIT_RUNS: AtomicU32 = AtomicU32::new(0);
    static M_RUNS: AtomicU32 = AtomicU32::new(0);
    static M_RAN_UNINITIALIZED: AtomicU32 = AtomicU32::new(0);
    fn clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        CLINIT_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn m(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        if CLINIT_RUNS.load(Ordering::SeqCst) == 0 {
            M_RAN_UNINITIALIZED.fetch_add(1, Ordering::SeqCst);
        }
        M_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const OWNER: &str = "cratonvm/i13l5/StrictStaticNativeOwner";
    let (shared, owner) = vm_with_owner(true, OWNER, clinit, m);
    assert!(static_by_name_init_enforced(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "i13-l5-strict");
    for _ in 0..2 {
        let result =
            invoke_static_or_native(&shared, &mut thread, OWNER, "m", "()V", &[], "test");
        assert!(result.is_ok(), "the call raised: {result:?}");
    }
    assert_eq!(
        M_RUNS.load(Ordering::SeqCst),
        2,
        "both calls run the native"
    );
    assert_eq!(
        M_RAN_UNINITIALIZED.load(Ordering::SeqCst),
        0,
        "the native ran before its class's <clinit>"
    );
    assert_eq!(
        CLINIT_RUNS.load(Ordering::SeqCst),
        1,
        "<clinit> runs exactly once"
    );
    assert!(crate::vm::is_class_initialized_via_manager(&shared, owner));
}

/// `--compatible` initializes through the static entry exactly when the
/// policy says so (not yet: the census decides), and the native runs
/// either way. When `static_by_name_init_enforced` flips, this test holds
/// unchanged.
#[test]
fn the_static_entry_follows_the_policy_under_compatible() {
    static CLINIT_RUNS: AtomicU32 = AtomicU32::new(0);
    static M_RUNS: AtomicU32 = AtomicU32::new(0);
    fn clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        CLINIT_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn m(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        M_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const OWNER: &str = "cratonvm/i13l5/CompatibleStaticNativeOwner";
    let (shared, owner) = vm_with_owner(false, OWNER, clinit, m);
    let mut thread = JvmThread::new(ThreadId(0), "i13-l5-compatible");
    let result = invoke_static_or_native(&shared, &mut thread, OWNER, "m", "()V", &[], "test");
    assert!(result.is_ok(), "the call raised: {result:?}");
    assert_eq!(M_RUNS.load(Ordering::SeqCst), 1);
    let enforced = static_by_name_init_enforced(&shared);
    assert_eq!(CLINIT_RUNS.load(Ordering::SeqCst) == 1, enforced);
    assert_eq!(
        crate::vm::is_class_initialized_via_manager(&shared, owner),
        enforced
    );
}

/// The every-mode helper the JIT funnel tail shares: it initializes the
/// declaring class of a registered static native, refreshes the argument
/// copy, and leaves a triple no native carries alone.
#[test]
fn the_shared_helper_initializes_in_every_mode_and_keeps_the_arguments() {
    static CLINIT_RUNS: AtomicU32 = AtomicU32::new(0);
    fn clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        CLINIT_RUNS.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn m(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    const OWNER: &str = "cratonvm/i13l5/HelperStaticNativeOwner";
    let (shared, owner) = vm_with_owner(false, OWNER, clinit, m);
    let mut thread = JvmThread::new(ThreadId(0), "i13-l5-helper");
    let mut args = [Value::Int(7), Value::Object(None)];
    let pins_before = thread.native_pin_roots.len();
    init_static_native_owner(
        &shared,
        &mut thread,
        None,
        OWNER,
        "cratonvmI13L5NoSuchNative",
        "(IJ)V",
        &mut args,
    )
    .expect("nothing to initialize");
    assert_eq!(
        CLINIT_RUNS.load(Ordering::SeqCst),
        0,
        "a triple no native carries initializes nothing here"
    );
    init_static_native_owner(&shared, &mut thread, None, OWNER, "m", "()V", &mut args)
        .expect("the initializer succeeds");
    assert_eq!(CLINIT_RUNS.load(Ordering::SeqCst), 1);
    assert!(crate::vm::is_class_initialized_via_manager(&shared, owner));
    assert!(matches!(args, [Value::Int(7), Value::Object(None)]));
    assert_eq!(thread.native_pin_roots.len(), pins_before, "pins released");
}

/// A reviewed VM-service receiver is never initialized under
/// `--real-jdk`, as the interpreter's native route leaves it; `Compatible`
/// keeps the route's historical answer.
#[test]
fn a_reviewed_vm_service_receiver_is_left_uninitialized_under_jdk_only() {
    fn nop(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    const SERVICE: &str = "java/lang/reflect/Proxy$Dispatch";
    assert!(cratonvm_native_api::no_image_receiver::is_reviewed_vm_service_receiver(SERVICE));
    const OWNER: &str = "cratonvm/i13l5/ServiceShapedOwner";
    let (strict, strict_owner) = vm_with_owner(true, OWNER, nop, nop);
    assert_eq!(
        static_owner_init_pending(&strict, strict_owner, SERVICE, "m", "()V"),
        None
    );
    assert_eq!(
        static_owner_init_pending(&strict, strict_owner, OWNER, "m", "()V"),
        Some(strict_owner)
    );
    let (compatible, owner) = vm_with_owner(false, OWNER, nop, nop);
    assert_eq!(
        static_owner_init_pending(&compatible, owner, SERVICE, "m", "()V"),
        Some(owner)
    );
}

/// The by-name `ClassLoader` intercepts (`setDefaultAssertionStatus`,
/// null-name `getResource*`) apply to a ClassLoader receiver only: a user
/// type's own method of that name and descriptor, instance or static, must
/// run. Probe: `UserSetDefaultAssertionStatus.java`.
#[test]
fn only_a_class_loader_receiver_takes_the_class_loader_intercepts() {
    fn nop(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        Ok(None)
    }
    let (shared, user) = vm_with_owner(false, "cratonvm/i13l5/NotALoader", nop, nop);
    assert!(!receiver_is_class_loader(&shared, &[]));
    assert!(
        !receiver_is_class_loader(&shared, &[Value::Int(1)]),
        "a static call has no receiver"
    );
    assert!(!receiver_is_class_loader(&shared, &[Value::Object(None)]));
    let plain = shared.mem.heap.alloc_object(user, 0);
    let call = [Value::Object(Some(plain)), Value::Int(1)];
    assert!(!receiver_is_class_loader(&shared, &call), "a user type");
    let loader = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/ClassLoader");
    // A stripped test VM may have no `ClassLoader` to subclass.
    let Some(loader) = loader else {
        return;
    };
    shared
        .classes
        .class_manager
        .write()
        .set_superclass(user, Some(loader));
    assert!(
        receiver_is_class_loader(&shared, &call),
        "a ClassLoader subclass"
    );
}
