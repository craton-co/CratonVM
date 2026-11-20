use super::{ByNameKind, NativeContextImpl, NativeInvokeAccess, SharedVm};
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

/// `int m()`: `bipush <value>; ireturn`.
fn int_m(flags: MethodAccessFlags, value: u8) -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from("m"),
        descriptor: Arc::from("()I"),
        access_flags: flags,
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0x10, value, 0xAC]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

fn define(
    shared: &SharedVm,
    name: &str,
    flags: ClassAccessFlags,
    superclass: Option<ClassId>,
    methods: Vec<ClassFileMethod>,
) -> ClassId {
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
        access_flags: flags,
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
            source: Arc::from("i16-l3-test"),
        },
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

/// A static by-name call has no receiver: the receiver-keyed arms are
/// asked for `Virtual` only, and every one of them in
/// `invoke_or_native_impl` is gated on that answer, including the
/// receiver-aware native owner helper.
#[test]
fn only_a_virtual_by_name_call_asks_the_receiver_keyed_arms() {
    assert!(ByNameKind::Virtual.may_have_receiver());
    assert!(!ByNameKind::Static.may_have_receiver());
    let src = concat!(
        include_str!("../vm_exec/mod.rs"),
        "\n",
        include_str!("../vm_exec/method.rs"),
        "\n",
        include_str!("../vm_exec/report_pad_access.rs"),
        "\n",
        include_str!("../vm_exec/diagnose_sync_on_value_based_class.rs"),
        "\n",
        include_str!("../vm_exec/thread_interrupted_field_slot_of.rs"),
        "\n",
        include_str!("../vm_exec/clear_breakpoints_of_redefined_class.rs"),
        "\n",
        include_str!("../vm_exec/load_class.rs"),
        "\n",
        include_str!("../vm_exec/native_new_initializes_class_first.rs"),
        "\n",
        include_str!("../vm_exec/new_object.rs"),
        "\n",
        include_str!("../vm_exec/gen_referent_read_keep_alive.rs"),
        "\n",
        include_str!("../vm_exec/append_native_standin_frames.rs"),
        "\n",
        include_str!("../vm_exec/unloaded_native_libraries.rs"),
        "\n",
        include_str!("../vm_exec/extract_annotations_from_attributes.rs"),
        "\n",
        include_str!("../vm_exec/invoke_static_settling_with_first.rs"),
        "\n",
        include_str!("../vm_exec/invoke_special_shared.rs"),
        "\n",
        include_str!("../vm_exec/proxy_resolve_declaring_method.rs"),
        "\n",
        include_str!("../vm_exec/invoke_on_class_shared_inner.rs"),
        "\n",
        include_str!("../vm_exec/jni_table_native_call.rs")
    )
    .replace("\r\n", "\n");
    let start = src
        .find("\npub(in crate::vm::vm_exec) fn invoke_or_native_impl_walk(")
        .unwrap_or(0);
    assert!(start > 0, "invoke_or_native_impl_walk moved");
    let body = &src[start..];
    let body = &body[..body.find("\n\x7d\n").unwrap_or(0)];
    assert_eq!(body.matches("if receiver_arms").count(), 9);
    assert!(body.contains(
        "by_name_native_owner(&cm, Some(receiver_cid), effective_class, receiver_arms)"
    ));
    assert!(body.contains("let receiver_arms = kind.may_have_receiver();"));
}

/// An abstract host `H` with a private `int m()` captured as a
/// `REF_invokeSpecial` implementation, and a receiver subclass `S` that
/// declares its own `int m()`: a native's `invoke_virtual` of the SAM runs
/// `H.m` (7), never `S.m` (9) — the interpreter's rule, which the native
/// copy's by-name dispatch retargeted onto the receiver's class.
#[test]
fn a_native_callback_runs_the_invoke_special_implementation_on_its_owner() {
    const H: &str = "cratonvm/i16l3/SpecialHost";
    const S: &str = "cratonvm/i16l3/SpecialSub";
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let host = define(
        &shared,
        H,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER | ClassAccessFlags::ABSTRACT,
        None,
        vec![int_m(MethodAccessFlags::PRIVATE, 7)],
    );
    let sub = define(
        &shared,
        S,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        Some(host),
        vec![int_m(MethodAccessFlags::PUBLIC, 9)],
    );
    let proxy = shared.alloc_lambda_proxy_id();
    shared.classes.lambda_proxies.write().insert(
        proxy,
        Arc::new(LambdaCallSite {
            functional_interface: Arc::from("java/util/function/IntSupplier"),
            functional_interface_id: None,
            sam_method_name: Arc::from("getAsInt"),
            sam_descriptor: Arc::from("()I"),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeSpecial,
                class_name: Arc::from(H),
                member_name: Arc::from("m"),
                descriptor: Arc::from("()I"),
            },
            instantiated_descriptor: Arc::from("()I"),
            capture_types: vec!['L'],
            proxy_class_id: proxy,
            serializable_flag: false,
        }),
    );
    let receiver = shared.mem.heap.alloc_object(sub, 0);
    let lambda = shared.mem.heap.alloc_object(proxy, 1);
    if shared.mem.heap.class_id_of(lambda) != proxy {
        return; // This heap cannot mint an object of a proxy id.
    }
    shared
        .mem
        .heap
        .set_field(lambda, 0, Value::Object(Some(receiver)));
    let mut thread = JvmThread::new(ThreadId(0), "i16-l3-native-lambda");
    let pins_before = thread.native_pin_roots.len();
    let mut ctx = NativeContextImpl {
        shared: &shared,
        thread: &mut thread,
    };
    let result = NativeInvokeAccess::invoke_virtual(&mut ctx, lambda, "getAsInt", "()I", &[]);
    assert!(
        matches!(result, Ok(Some(Value::Int(7)))),
        "the private H.m runs, not S.m: {result:?}"
    );
    assert_eq!(thread.native_pin_roots.len(), pins_before, "pins released");
}
