use super::{NativeContextImpl, NativeInvokeAccess, SharedVm};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::types::Value;
use cratonvm_reader::attribute::{Attribute, CodeAttribute};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_reader::LazyAttribute;
use std::sync::Arc;

fn method(
    name: &str,
    descriptor: &str,
    flags: MethodAccessFlags,
    max_locals: u16,
    code: Vec<u8>,
) -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        access_flags: flags,
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals,
                code: cratonvm_reader::ByteView::from_vec(code),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// An already-initialized application class holding `methods`.
fn define(shared: &SharedVm, name: &str, methods: Vec<ClassFileMethod>) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
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
        origin: cratonvm_classloading::ClassOrigin::ApplicationClassPath {
            source: Arc::from("i17-l4-test"),
        },
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            cratonvm_classloading::CLASS_INIT_INITIALIZED,
        )),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

/// A capture-free lambda proxy `iface.sam sam_desc` over `impl_handle`.
fn proxy(
    shared: &SharedVm,
    iface: &str,
    sam: &str,
    sam_desc: &str,
    inst_desc: &str,
    impl_handle: MethodHandle,
) -> ClassId {
    let id = shared.alloc_lambda_proxy_id();
    shared.classes.lambda_proxies.write().insert(
        id,
        Arc::new(LambdaCallSite {
            functional_interface: Arc::from(iface),
            functional_interface_id: None,
            sam_method_name: Arc::from(sam),
            sam_descriptor: Arc::from(sam_desc),
            impl_handle,
            instantiated_descriptor: Arc::from(inst_desc),
            capture_types: vec![],
            proxy_class_id: id,
            serializable_flag: false,
        }),
    );
    id
}

#[test]
fn virtual_table_admission_respects_the_live_registration_kind_in_both_modes() {
    use cratonvm_native_api::{InterpIntrinsic, NativeKind};
    use cratonvm_types::compat::CompatibilityMode;
    for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
        for kind in [
            NativeKind::Bridge,
            NativeKind::SyntheticStub,
            NativeKind::Intrinsic,
        ] {
            let mut config = crate::config::VmConfig::default();
            config.compatibility_mode = mode;
            config.use_synthetic_jdk = false;
            let mut vm = SharedVm::new(config);
            vm.natives.native_methods.register(
                "cratonvm/ffmstw/TableDispatch",
                "length",
                "()I",
                |_, _| Ok(Some(Value::Int(99))),
                kind,
            );
            let shared = Arc::new(vm);
            let owner = define(
                &shared,
                "cratonvm/ffmstw/TableDispatch",
                vec![method(
                    "length",
                    "()I",
                    MethodAccessFlags::PUBLIC,
                    1,
                    vec![0x10, 7, 0xAC],
                )],
            );
            let cm = shared.classes.class_manager.read();
            let class = cm.get_class(owner).unwrap();
            assert_eq!(
                super::table_intrinsic_admitted(
                    &shared,
                    class,
                    &class.methods[0],
                    InterpIntrinsic::StringBuilderLength,
                ),
                kind == NativeKind::Intrinsic,
                "{mode:?} {kind:?}"
            );
        }
    }
}

#[test]
fn native_java_tail_obeys_resolved_bytecode_in_both_modes() {
    use cratonvm_native_api::NativeKind;
    use cratonvm_types::compat::CompatibilityMode;

    const HOST: &str = "cratonvm/ffmstw/TailDispatch";
    for host in [
        HOST,
        "jdk/internal/foreign/ArenaImpl",
        "java/lang/foreign/Arena",
    ] {
        for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
            for kind in [
                NativeKind::Bridge,
                NativeKind::SyntheticStub,
                NativeKind::Intrinsic,
            ] {
                let mut config = crate::config::VmConfig::default();
                config.compatibility_mode = mode;
                config.use_synthetic_jdk = false;
                let mut vm = SharedVm::new(config);
                vm.natives.native_methods.register(
                    host,
                    "m",
                    "()I",
                    |_, _| Ok(Some(Value::Int(99))),
                    kind,
                );
                let shared = Arc::new(vm);
                let owner = define(
                    &shared,
                    host,
                    vec![method(
                        "m",
                        "()I",
                        MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
                        0,
                        vec![0x10, 7, 0xAC],
                    )],
                );
                if host == "java/lang/foreign/Arena" {
                    let mut cm = shared.classes.class_manager_write();
                    cm.class_store.get_mut(owner).unwrap().access_flags =
                        ClassAccessFlags::PUBLIC
                            | ClassAccessFlags::INTERFACE
                            | ClassAccessFlags::ABSTRACT;
                }
                let mut thread = JvmThread::new(ThreadId(0), "ffm-stw-tail-dispatch");
                let expected = if kind == NativeKind::Intrinsic { 99 } else { 7 };
                for round in 0..2 {
                    let result = super::invoke_on_class_shared(
                        &shared,
                        &mut thread,
                        owner,
                        "m",
                        "()I",
                        &[],
                    );
                    assert!(
                        matches!(result, Ok(Some(Value::Int(value))) if value == expected),
                        "{mode:?} {kind:?} round {round}: {result:?}"
                    );
                    let resolved_static = super::invoke_static_shared_on_class(
                        &shared,
                        &mut thread,
                        owner,
                        host,
                        "m",
                        "()I",
                        &[],
                    );
                    assert!(
                        matches!(resolved_static, Ok(Some(Value::Int(value))) if value == expected),
                        "resolved-static {mode:?} {kind:?} round {round}: {resolved_static:?}",
                    );
                    let by_name = super::invoke_static_or_native(
                        &shared,
                        &mut thread,
                        host,
                        "m",
                        "()I",
                        &[],
                        "ffm-stw-tail-test",
                    );
                    assert!(
                        matches!(by_name, Ok(Some(Value::Int(value))) if value == expected),
                        "by-name {mode:?} {kind:?} round {round}: {by_name:?}"
                    );
                }
            }
        }
    }
}

/// `REF_invokeStatic`: a native's `invoke_virtual` of the SAM runs the
/// static implementation through the interpreter's dispatcher, twice
/// (the second call is served by the cached owner).
#[test]
fn a_native_callback_runs_a_static_implementation() {
    const HOST: &str = "cratonvm/i17l4/StaticHost";
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let flags = MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC;
    // `static int m()`: `bipush 7; ireturn`.
    define(
        &shared,
        HOST,
        vec![method("m", "()I", flags, 0, vec![0x10, 7, 0xAC])],
    );
    let lambda_class = proxy(
        &shared,
        "java/util/function/IntSupplier",
        "getAsInt",
        "()I",
        "()I",
        MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from(HOST),
            member_name: Arc::from("m"),
            descriptor: Arc::from("()I"),
        },
    );
    let lambda = shared.mem.heap.alloc_object(lambda_class, 0);
    if shared.mem.heap.class_id_of(lambda) != lambda_class {
        return; // This heap cannot mint an object of a proxy id.
    }
    let mut thread = JvmThread::new(ThreadId(0), "i17-l4-native-lambda");
    let pins_before = thread.native_pin_roots.len();
    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    for round in 0..2 {
        let result =
            NativeInvokeAccess::invoke_virtual(&mut ctx, lambda, "getAsInt", "()I", &[]);
        assert!(
            matches!(result, Ok(Some(Value::Int(7)))),
            "round {round}: {result:?}"
        );
    }
    assert_eq!(thread.native_pin_roots.len(), pins_before, "pins released");
}

/// `REF_newInvokeSpecial`: a `Supplier` answers the new object, and a
/// `Runnable` over the same constructor answers nothing (a `void` SAM
/// discards the object).
#[test]
fn a_native_callback_runs_a_constructor_implementation() {
    const MADE: &str = "cratonvm/i17l4/Made";
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    // `<init>()V`: `return`.
    let made = define(
        &shared,
        MADE,
        vec![method(
            "<init>",
            "()V",
            MethodAccessFlags::PUBLIC,
            1,
            vec![0xB1],
        )],
    );
    let ctor = || MethodHandle {
        kind: MethodHandleKind::NewInvokeSpecial,
        class_name: Arc::from(MADE),
        member_name: Arc::from("<init>"),
        descriptor: Arc::from("()V"),
    };
    let supplier = proxy(
        &shared,
        "java/util/function/Supplier",
        "get",
        "()Ljava/lang/Object;",
        "()Lcratonvm/i17l4/Made;",
        ctor(),
    );
    let runnable = proxy(&shared, "java/lang/Runnable", "run", "()V", "()V", ctor());
    let supplier_obj = shared.mem.heap.alloc_object(supplier, 0);
    let runnable_obj = shared.mem.heap.alloc_object(runnable, 0);
    if shared.mem.heap.class_id_of(supplier_obj) != supplier
        || shared.mem.heap.class_id_of(runnable_obj) != runnable
    {
        return; // This heap cannot mint an object of a proxy id.
    }
    let mut thread = JvmThread::new(ThreadId(0), "i17-l4-native-ctor");
    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let got = NativeInvokeAccess::invoke_virtual(
        &mut ctx,
        supplier_obj,
        "get",
        "()Ljava/lang/Object;",
        &[],
    );
    match got {
        Ok(Some(Value::Object(Some(o)))) => {
            assert_eq!(shared.mem.heap.class_id_of(o), made, "a new Made");
        }
        other => panic!("Supplier.get of Made::new: {other:?}"),
    }
    let ran = NativeInvokeAccess::invoke_virtual(&mut ctx, runnable_obj, "run", "()V", &[]);
    assert!(
        matches!(ran, Ok(None)),
        "a void SAM answers nothing: {ran:?}"
    );
}
