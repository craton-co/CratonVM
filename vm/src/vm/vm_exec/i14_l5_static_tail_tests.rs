use super::{
    invoke_static_on_class_no_retarget, invoke_static_or_native, invoke_static_shared_on_class,
    SharedVm,
};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::error::MethodCallResult;
use crate::native::registry::{NativeCallback, NativeContext};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;
use cratonvm_reader::attribute::{Attribute, CodeAttribute};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::LazyAttribute;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

fn config(jdk_only: bool) -> crate::config::VmConfig {
    let mut config = crate::config::VmConfig::default();
    if jdk_only {
        config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
        // `JdkOnly` with the synthetic library is a rejected pair.
        config.use_synthetic_jdk = false;
    }
    config
}

/// `static int <name>()`: `bipush 7; ireturn`.
fn static_seven(name: &str) -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from(name),
        descriptor: Arc::from("()I"),
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 0,
                code: cratonvm_reader::ByteView::from_vec(vec![0x10, 7, 0xAC]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// `static native <clinit>()V`, answered by a registered `Bridge`.
fn native_clinit() -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from("<clinit>"),
        descriptor: Arc::from("()V"),
        access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
        attributes: vec![],
    }
}

/// Define `name` as a LOADED class (not initialized). `vm_internal`
/// makes it a VM-internal class with a native method, which the
/// loaded-class shortcut of `invoke_or_native_impl` hands to its
/// load-then-dispatch fallback (`Class::dispatch_lacks_class_file`).
fn define(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    methods: Vec<ClassFileMethod>,
    vm_internal: bool,
) -> ClassId {
    let origin = if vm_internal {
        cratonvm_classloading::ClassOrigin::VmInternal
    } else {
        cratonvm_classloading::ClassOrigin::ApplicationClassPath {
            source: Arc::from("i14-l5-test"),
        }
    };
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods,
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
        origin,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    if superclass.is_some() {
        cm.set_superclass(id, superclass);
    }
    id
}

/// `parent` declares `static int m()` and `child extends parent` declares
/// `static int own()`; each has a registered native `<clinit>`. Neither
/// class is initialized.
fn parent_and_child(
    jdk_only: bool,
    vm_internal: bool,
    parent: &str,
    child: &str,
    parent_clinit: NativeCallback,
    child_clinit: NativeCallback,
) -> (Arc<SharedVm>, ClassId, ClassId) {
    let mut vm = SharedVm::new(config(jdk_only));
    for (owner, callback) in [(parent, parent_clinit), (child, child_clinit)] {
        vm.natives.native_methods.register(
            owner,
            "<clinit>",
            "()V",
            callback,
            cratonvm_native_api::NativeKind::Bridge,
        );
    }
    let shared = Arc::new(vm);
    let p = define(
        &shared,
        parent,
        None,
        vec![native_clinit(), static_seven("m")],
        vm_internal,
    );
    let c = define(
        &shared,
        child,
        Some(p),
        vec![native_clinit(), static_seven("own")],
        vm_internal,
    );
    assert!(!crate::vm::is_class_initialized_via_manager(&shared, p));
    assert!(!crate::vm::is_class_initialized_via_manager(&shared, c));
    (shared, p, c)
}

fn initialized(shared: &SharedVm, id: ClassId) -> bool {
    crate::vm::is_class_initialized_via_manager(shared, id)
}

/// `C.m()` through the static entry runs `P.<clinit>` only, on the
/// loaded-class shortcut; a static `C` declares itself still initializes
/// `C`.
#[test]
fn an_inherited_static_initializes_only_its_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i14l5/LoadedParent";
    const C: &str = "cratonvm/i14l5/LoadedChild";
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i14-l5-loaded");
    for _ in 0..2 {
        let result = invoke_static_or_native(&shared, &mut thread, C, "m", "()I", &[], "test");
        assert!(
            matches!(result, Ok(Some(Value::Int(7)))),
            "C.m() runs P.m: {result:?}"
        );
    }
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1, "P.<clinit> runs once");
    assert_eq!(
        C_CLINIT.load(Ordering::SeqCst),
        0,
        "an inherited static never initializes the class it is named through"
    );
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));

    let own = invoke_static_or_native(&shared, &mut thread, C, "own", "()I", &[], "test");
    assert!(matches!(own, Ok(Some(Value::Int(7)))), "C.own(): {own:?}");
    assert_eq!(
        C_CLINIT.load(Ordering::SeqCst),
        1,
        "a static C declares initializes C"
    );
    assert!(initialized(&shared, c));
}

/// Stage 4 of the by-name-kind proposal (interpreter round i1 wave 52,
/// lane L4): a static tail whose first argument is an instance of a
/// concrete subclass of the ABSTRACT owner runs the owner's static, with
/// `VmConfig::by_name_static_kind` on (no retarget walk) and off (the
/// walk, which finds the method static and keeps the owner) alike.
#[test]
fn a_static_tail_never_retargets_onto_its_first_argument_with_or_without_the_kind() {
    /// `static int m(Object)`: `bipush 7; ireturn`.
    fn static_m() -> ClassFileMethod {
        ClassFileMethod {
            name: Arc::from("m"),
            descriptor: Arc::from("(Ljava/lang/Object;)I"),
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
                CodeAttribute {
                    max_stack: 1,
                    max_locals: 1,
                    code: cratonvm_reader::ByteView::from_vec(vec![0x10, 7, 0xAC]),
                    exception_table: vec![],
                    attributes: vec![],
                },
            )))],
        }
    }
    for kind_on in [true, false] {
        let mut cfg = config(false);
        cfg.by_name_static_kind = kind_on;
        let shared = Arc::new(SharedVm::new(cfg));
        let a_name = if kind_on {
            "cratonvm/i52l4/AbsOn"
        } else {
            "cratonvm/i52l4/AbsOff"
        };
        let b_name = if kind_on {
            "cratonvm/i52l4/ConcOn"
        } else {
            "cratonvm/i52l4/ConcOff"
        };
        let a = define(&shared, a_name, None, vec![static_m()], false);
        shared
            .classes
            .class_manager
            .write()
            .class_store
            .get_mut(a)
            .expect("just defined")
            .access_flags |= ClassAccessFlags::ABSTRACT;
        let b = define(&shared, b_name, Some(a), vec![], false);
        let receiver_like = shared.mem.heap.alloc_object(b, 0);
        let mut thread = JvmThread::new(ThreadId(0), "i52-l4-static-kind");
        let result = invoke_static_or_native(
            &shared,
            &mut thread,
            a_name,
            "m",
            "(Ljava/lang/Object;)I",
            &[Value::Object(Some(receiver_like))],
            "test",
        );
        assert!(
            matches!(result, Ok(Some(Value::Int(7)))),
            "A.m(b) runs A.m (kind {kind_on}): {result:?}"
        );
    }
}

/// The same through the load-then-dispatch fallback (`invoke_static_shared`).
#[test]
fn the_load_then_dispatch_fallback_initializes_only_the_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i14l5/InternalParent";
    const C: &str = "cratonvm/i14l5/InternalChild";
    let (shared, p, c) = parent_and_child(false, true, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i14-l5-fallback");
    let result = invoke_static_or_native(&shared, &mut thread, C, "m", "()I", &[], "test");
    assert!(
        matches!(result, Ok(Some(Value::Int(7)))),
        "C.m() runs P.m: {result:?}"
    );
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 0);
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));
}

/// The resolved-owner form the JIT tail takes for a loader-faithful owner
/// (`invoke_static_shared_on_class`) initializes the declaring class too.
#[test]
fn the_resolved_owner_form_initializes_only_the_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i14l5/OnClassParent";
    const C: &str = "cratonvm/i14l5/OnClassChild";
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i14-l5-on-class");
    let result = invoke_static_shared_on_class(&shared, &mut thread, c, C, "m", "()I", &[]);
    assert!(
        matches!(result, Ok(Some(Value::Int(7)))),
        "C.m() runs P.m: {result:?}"
    );
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 0);
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));
}

/// The loader-faithful `REF_invokeStatic` lambda arms
/// (`invoke_static_on_class_no_retarget`, wave 15) initialize the class
/// that declares the implementation before it runs — once — and never the
/// class an inherited static is named through; a static the owner itself
/// declares initializes the owner.
#[test]
fn the_loader_faithful_lambda_static_arm_initializes_the_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i15l5/LambdaImplParent";
    const C: &str = "cratonvm/i15l5/LambdaImplChild";
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i15-l5-lambda-static");
    let pins_before = thread.native_pin_roots.len();
    for _ in 0..2 {
        let result =
            invoke_static_on_class_no_retarget(&shared, &mut thread, c, C, "m", "()I", &[]);
        assert!(
            matches!(result, Ok(Some(Value::Int(7)))),
            "C::m runs P.m: {result:?}"
        );
    }
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1, "P.<clinit> runs once");
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 0);
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));

    let own = invoke_static_on_class_no_retarget(&shared, &mut thread, c, C, "own", "()I", &[]);
    assert!(matches!(own, Ok(Some(Value::Int(7)))), "C::own: {own:?}");
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 1);
    assert!(initialized(&shared, c));
    assert_eq!(thread.native_pin_roots.len(), pins_before, "pins released");
}

/// `NativeContext::invoke` of an inherited static named through its
/// subclass under `--real-jdk` (`native_invoke_static_tail_enforced`):
/// only the declaring class is initialized; a static the named class
/// declares still initializes it (wave 15).
#[test]
fn native_context_invoke_of_an_inherited_static_initializes_only_the_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i15l5/NativeInvokeParent";
    const C: &str = "cratonvm/i15l5/NativeInvokeChild";
    let (shared, p, c) = parent_and_child(true, false, P, C, p_clinit, c_clinit);
    assert!(super::native_invoke_static_tail_enforced(&shared));
    let mut thread = JvmThread::new(ThreadId(0), "i15-l5-native-invoke");
    let mut ctx = super::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    for _ in 0..2 {
        let result = super::NativeInvokeAccess::invoke(&mut ctx, C, "m", "()I", &[]);
        assert!(
            matches!(result, Ok(Some(Value::Int(7)))),
            "C.m() runs P.m: {result:?}"
        );
    }
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1, "P.<clinit> runs once");
    assert_eq!(
        C_CLINIT.load(Ordering::SeqCst),
        0,
        "an inherited static never initializes the class it is named through"
    );
    let own = super::NativeInvokeAccess::invoke(&mut ctx, C, "own", "()I", &[]);
    assert!(matches!(own, Ok(Some(Value::Int(7)))), "C.own(): {own:?}");
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 1);
    assert!(initialized(&shared, p));
    assert!(initialized(&shared, c));
}

/// The same door under `--compatible` keeps the instance tail (the named
/// class is initialized too) until the census flips
/// `native_invoke_static_tail_enforced`; the call runs either way, and
/// this test holds unchanged across the flip.
#[test]
fn native_context_invoke_of_an_inherited_static_follows_the_policy_under_compatible() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i15l5/CompatNativeInvokeParent";
    const C: &str = "cratonvm/i15l5/CompatNativeInvokeChild";
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let enforced = super::native_invoke_static_tail_enforced(&shared);
    let mut thread = JvmThread::new(ThreadId(0), "i15-l5-native-invoke-compat");
    let mut ctx = super::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let result = super::NativeInvokeAccess::invoke(&mut ctx, C, "m", "()I", &[]);
    assert!(
        matches!(result, Ok(Some(Value::Int(7)))),
        "C.m() runs P.m: {result:?}"
    );
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(C_CLINIT.load(Ordering::SeqCst) == 0, enforced);
    assert!(initialized(&shared, p));
    assert_eq!(initialized(&shared, c), !enforced);
}

/// `NativeContext::invoke_static` — a `findStatic` handle's dispatch
/// (wave 16, lane L3) — initializes only the declaring class of an
/// inherited static, in `--compatible` too, once; a static the named
/// class declares initializes it.
#[test]
fn native_context_invoke_static_initializes_only_the_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i16l3/MhStaticParent";
    const C: &str = "cratonvm/i16l3/MhStaticChild";
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i16-l3-invoke-static");
    let mut ctx = super::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    for _ in 0..2 {
        let result = super::NativeInvokeAccess::invoke_static(&mut ctx, C, "m", "()I", &[]);
        assert!(
            matches!(result, Ok(Some(Value::Int(7)))),
            "C.m() runs P.m: {result:?}"
        );
    }
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1, "P.<clinit> runs once");
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 0);
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));
    let own = super::NativeInvokeAccess::invoke_static(&mut ctx, C, "own", "()I", &[]);
    assert!(matches!(own, Ok(Some(Value::Int(7)))), "C.own(): {own:?}");
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 1);
    assert!(initialized(&shared, c));
}

/// The resolved-owner form (`invoke_static_by_class_id`, a `--real-jdk`
/// handle's recorded owner) initializes the declaring class, not the
/// owner it is handed.
#[test]
fn native_context_invoke_static_by_class_id_initializes_only_the_declaring_class() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i16l3/MhStaticIdParent";
    const C: &str = "cratonvm/i16l3/MhStaticIdChild";
    let (shared, p, c) = parent_and_child(true, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i16-l3-invoke-static-id");
    let mut ctx = super::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let result =
        super::NativeInvokeAccess::invoke_static_by_class_id(&mut ctx, c, C, "m", "()I", &[]);
    assert!(
        matches!(result, Ok(Some(Value::Int(7)))),
        "C.m() runs P.m: {result:?}"
    );
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 0);
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));
}

/// Interpreter round i1 wave 20, lane L3: a `findStatic` handle's
/// settling dispatch (`invoke_static_settling`) initializes exactly what
/// `invoke_static` does — the declaring class of an inherited static,
/// once — and reports the owner settled after that call. A later call
/// handing the owner back skips the initialization check and still runs
/// the method; a remembered owner that is not the one the call resolves
/// is ignored.
#[test]
fn a_settling_static_dispatch_initializes_the_declaring_class_once_then_settles() {
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i20l3/MhSettleParent";
    const C: &str = "cratonvm/i20l3/MhSettleChild";
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i20-l3-invoke-static-settling");
    let mut ctx = super::NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let (first, settled) = super::NativeInvokeAccess::invoke_static_settling(
        &mut ctx,
        None,
        None,
        C,
        "m",
        "()I",
        &[],
    );
    assert!(
        matches!(first, Ok(Some(Value::Int(7)))),
        "C.m() runs P.m: {first:?}"
    );
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 0);
    assert_eq!(settled, Some(c), "nothing is left to initialize for C.m");
    // Settled: by name and by id alike, no initialization and the same answer.
    for owner in [None, Some(c)] {
        let (again, still) = super::NativeInvokeAccess::invoke_static_settling(
            &mut ctx,
            owner,
            settled,
            C,
            "m",
            "()I",
            &[],
        );
        assert!(matches!(again, Ok(Some(Value::Int(7)))), "{again:?}");
        assert_eq!(still, Some(c));
    }
    // A remembered owner that is not this call's owner is not trusted:
    // `C.own()` is C's own static, so the call initializes C.
    let (own, own_settled) = super::NativeInvokeAccess::invoke_static_settling(
        &mut ctx,
        None,
        Some(p),
        C,
        "own",
        "()I",
        &[],
    );
    assert!(matches!(own, Ok(Some(Value::Int(7)))), "C.own(): {own:?}");
    assert_eq!(C_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(own_settled, Some(c));
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1, "P.<clinit> ran once");
    assert!(initialized(&shared, p));
    assert!(initialized(&shared, c));
}

/// Every by-name tail whose argument rooting wave 15 reshaped
/// (`invoke_by_class_id_shared` initializing and in steady state,
/// `invoke_shared`, `invoke_static_shared`, `invoke_special_shared`) runs
/// a method taking an object argument, initializes its class once, and
/// leaves the pin stack where it found it.
#[test]
fn the_by_name_tails_pass_an_object_argument_and_release_their_pins() {
    static CLINIT: AtomicU32 = AtomicU32::new(0);
    fn clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const OWNER: &str = "cratonvm/i15l5/ObjectArgOwner";
    const DESC: &str = "(Ljava/lang/Object;)I";
    let mut vm = SharedVm::new(config(false));
    vm.natives.native_methods.register(
        OWNER,
        "<clinit>",
        "()V",
        clinit,
        cratonvm_native_api::NativeKind::Bridge,
    );
    let shared = Arc::new(vm);
    // `static int k(Object)`: `bipush 7; ireturn`.
    let k = ClassFileMethod {
        name: Arc::from("k"),
        descriptor: Arc::from(DESC),
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0x10, 7, 0xAC]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    };
    let owner = define(&shared, OWNER, None, vec![native_clinit(), k], false);
    let obj = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let args = [Value::Object(Some(obj))];
    let mut thread = JvmThread::new(ThreadId(0), "i15-l5-object-arg");
    let pins_before = thread.native_pin_roots.len();
    let seven = |result: &MethodCallResult| matches!(result, Ok(Some(Value::Int(7))));

    let first = super::invoke_by_class_id_shared(&shared, &mut thread, owner, "k", DESC, &args);
    assert!(seven(&first), "initializing call: {first:?}");
    assert_eq!(CLINIT.load(Ordering::SeqCst), 1);
    assert!(initialized(&shared, owner));
    assert_eq!(thread.native_pin_roots.len(), pins_before);

    let steady =
        super::invoke_by_class_id_shared(&shared, &mut thread, owner, "k", DESC, &args);
    assert!(seven(&steady), "steady-state call: {steady:?}");
    let by_name = super::invoke_shared(&shared, &mut thread, OWNER, "k", DESC, &args);
    assert!(seven(&by_name), "invoke_shared: {by_name:?}");
    let by_name_static =
        super::invoke_static_shared(&shared, &mut thread, OWNER, "k", DESC, &args);
    assert!(
        seven(&by_name_static),
        "invoke_static_shared: {by_name_static:?}"
    );
    let special = super::invoke_special_shared(&shared, &mut thread, OWNER, "k", DESC, &args);
    assert!(seven(&special), "invoke_special_shared: {special:?}");
    assert_eq!(CLINIT.load(Ordering::SeqCst), 1, "<clinit> runs once");
    assert_eq!(thread.native_pin_roots.len(), pins_before, "pins released");
}

/// A reviewed VM-service receiver is never initialized under `--real-jdk`
/// on the bytecode tail either, as `static_owner_init_pending` answers
/// for the interpreter's native route.
#[test]
fn a_reviewed_vm_service_owner_stays_uninitialized_on_the_static_tail() {
    static CLINIT: AtomicU32 = AtomicU32::new(0);
    fn clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const SERVICE: &str = "java/lang/reflect/Proxy$Dispatch";
    assert!(cratonvm_native_api::no_image_receiver::is_reviewed_vm_service_receiver(SERVICE));
    let mut vm = SharedVm::new(config(true));
    vm.natives.native_methods.register(
        SERVICE,
        "<clinit>",
        "()V",
        clinit,
        cratonvm_native_api::NativeKind::Bridge,
    );
    let shared = Arc::new(vm);
    let service = define(
        &shared,
        SERVICE,
        None,
        vec![native_clinit(), static_seven("cratonvmI14L5m")],
        false,
    );
    let mut thread = JvmThread::new(ThreadId(0), "i14-l5-service");
    let _ = invoke_static_or_native(
        &shared,
        &mut thread,
        SERVICE,
        "cratonvmI14L5m",
        "()I",
        &[],
        "test",
    );
    assert_eq!(CLINIT.load(Ordering::SeqCst), 0);
    assert!(!initialized(&shared, service));
}

/// A compiled `invokestatic C.m()` whose first execution reaches the JIT
/// dispatch door runs `P.<clinit>` only — the probe's shape — and the
/// second call is served without initializing `C` either.
#[test]
fn a_compiled_inherited_static_call_initializes_only_the_declaring_class() {
    if crate::runtime::env_cache::disable_jit() {
        return;
    }
    static P_CLINIT: AtomicU32 = AtomicU32::new(0);
    static C_CLINIT: AtomicU32 = AtomicU32::new(0);
    fn p_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        P_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn c_clinit(_: &mut dyn NativeContext, _: &[Value]) -> MethodCallResult {
        C_CLINIT.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    const P: &str = "cratonvm/i14l5/JitParent";
    const C: &str = "cratonvm/i14l5/JitChild";
    static INFO: cratonvm_jit::JitInvokeInfo = cratonvm_jit::JitInvokeInfo {
        class_name: C,
        method_name: "m",
        descriptor: "()I",
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    };
    let (shared, p, c) = parent_and_child(false, false, P, C, p_clinit, c_clinit);
    let mut thread = JvmThread::new(ThreadId(0), "i14-l5-jit");
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    let info_ptr = &INFO as *const cratonvm_jit::JitInvokeInfo as i64;
    let scope = crate::jit::helpers::set_jit_thread(&mut thread);
    // SAFETY: `vm_ptr` names the live VM above, `info_ptr` a `'static` call
    // site, the argument buffer is empty, and the JIT thread pointer is
    // installed for this OS thread — the contract compiled code satisfies.
    let first = unsafe { crate::jit::helpers::jit_invoke_dispatch(vm_ptr, info_ptr, 0, 0) };
    // SAFETY: as above.
    let second = unsafe { crate::jit::helpers::jit_invoke_dispatch(vm_ptr, info_ptr, 0, 0) };
    crate::jit::helpers::restore_jit_thread(scope);
    assert_eq!(first, 7, "the first call returns P.m's value");
    assert_eq!(second, 7, "the second call returns P.m's value");
    assert_eq!(P_CLINIT.load(Ordering::SeqCst), 1);
    assert_eq!(
        C_CLINIT.load(Ordering::SeqCst),
        0,
        "the compiled route initialized the symbolic owner"
    );
    assert!(initialized(&shared, p));
    assert!(!initialized(&shared, c));
}
