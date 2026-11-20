use super::declaring_class_self_locking_body_unswitched as lookup;
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::ClassId;
use std::sync::Arc;

const PARENT: &str = "cratonvm/test/r16tp/Parent";
const CHILD: &str = "cratonvm/test/r16tp/Child";

fn method(flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: flags,
        name: Arc::from("fill"),
        descriptor: Arc::from("()V"),
        attributes: vec![],
    }
}

/// `register_test_class` (`vm/tests.rs`) with a superclass.
fn register(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    methods: Vec<ClassFileMethod>,
) -> ClassId {
    use crate::classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    let mut cm = shared.classes.class_manager_write();
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
        access_flags: ClassAccessFlags::from_bits_truncate(0x0021),
        superclass,
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
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

fn body(self_locks: bool, wrapped: bool) -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // ret -- metadata-only fixture, never executed
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.self_locks_monitor = self_locks;
    cm.requires_wrapped_entry = wrapped;
    cm
}

fn publish(shared: &SharedVm, class: &str, id: ClassId, cm: cratonvm_jit::CompiledMethod) {
    shared.jit.jit_cache.write().put(
        Arc::from(class),
        Arc::from("fill"),
        Arc::from("()V"),
        id,
        cm,
    );
}

fn hierarchy(flags: MethodAccessFlags) -> (SharedVm, ClassId, ClassId) {
    let shared = SharedVm::new(VmConfig::default());
    let parent = register(&shared, PARENT, None, vec![method(flags)]);
    let child = register(&shared, CHILD, Some(parent), Vec::new());
    (shared, parent, child)
}

/// The `RuntimeException` receiver of `Throwable.fillInStackTrace()`.
#[test]
fn a_subclass_receiver_is_served_the_declaring_classs_self_locking_body() {
    let (shared, parent, child) =
        hierarchy(MethodAccessFlags::PUBLIC | MethodAccessFlags::SYNCHRONIZED);
    assert!(
        lookup(&shared, child, CHILD, "fill", "()V").is_none(),
        "nothing published"
    );
    publish(&shared, PARENT, parent, body(true, false));
    let (compiled, entry, _) =
        lookup(&shared, child, CHILD, "fill", "()V").expect("the declaring class's body");
    assert!(compiled.self_locks_monitor);
    assert_eq!(entry, compiled.entry_ptr() as usize);
    // Declared on the probe class: the probe key's own rule answers it.
    assert!(lookup(&shared, parent, PARENT, "fill", "()V").is_none());
}

/// Only what a raw door may CALL: a wrapped entry relies on the caller's
/// monitor wrapper, and a body that does not lock itself takes none.
#[test]
fn a_wrapped_or_unlocked_body_is_not_served() {
    let (shared, parent, child) =
        hierarchy(MethodAccessFlags::PUBLIC | MethodAccessFlags::SYNCHRONIZED);
    publish(&shared, PARENT, parent, body(false, true));
    assert!(
        lookup(&shared, child, CHILD, "fill", "()V").is_none(),
        "wrapped"
    );
    publish(&shared, PARENT, parent, body(false, false));
    assert!(
        lookup(&shared, child, CHILD, "fill", "()V").is_none(),
        "no self lock"
    );
}

/// Not this rule's question: an unsynchronized inherited method.
#[test]
fn an_unsynchronized_method_is_not_served() {
    let (shared, parent, child) = hierarchy(MethodAccessFlags::PUBLIC);
    publish(&shared, PARENT, parent, body(true, false));
    assert!(lookup(&shared, child, CHILD, "fill", "()V").is_none());
    assert!(
        lookup(&shared, child, CHILD, "other", "()V").is_none(),
        "no such method"
    );
}
