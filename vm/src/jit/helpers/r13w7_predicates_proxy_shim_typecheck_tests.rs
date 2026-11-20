use super::*;
use crate::classloading::{Class, ClassLoaderId, ClassState};
use crate::runtime::proxy::PROXY_FIELD_INTERFACES;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use std::sync::Arc;

const IFACE: &str = "cratonvm/r13w7/PredicatesIface";
const OTHER: &str = "cratonvm/r13w7/PredicatesOther";

fn define(
    shared: &SharedVm,
    name: &str,
    access_flags: ClassAccessFlags,
    num_total_fields: usize,
    register: bool,
) -> ClassId {
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
        access_flags,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields,
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
    if register {
        cm.register_class_name(ClassLoaderId::Application, name, id);
    }
    id
}

/// `(shim class, a Proxy$Instance whose slot-1 Class[] is {IFACE})`, or
/// `None` when this heap cannot mint the fixture.
fn fixture(shared: &SharedVm) -> Option<(ClassId, ObjectRef)> {
    let iface =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
    let iface_id = define(shared, IFACE, iface, 0, true);
    define(shared, OTHER, iface, 0, true);
    let shim = define(
        shared,
        "java/lang/reflect/Proxy$Instance",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        3,
        false,
    );
    let mirror = shared.mem.heap.alloc_object(shim, 0);
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .insert(mirror, iface_id);
    let interfaces =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    shared
        .mem
        .heap
        .set_array_element(interfaces, 0, Value::Object(Some(mirror)))
        .ok()?;
    let proxy = shared.mem.heap.alloc_object(shim, 3);
    if shared.mem.heap.class_id_of(proxy) != shim {
        return None;
    }
    shared.mem.heap.set_field(
        proxy,
        PROXY_FIELD_INTERFACES,
        Value::Object(Some(interfaces)),
    );
    Some((shim, proxy))
}

#[test]
fn a_bare_proxy_instance_is_admitted_and_not_memoized() {
    if !jit_typecheck_instance_rules() {
        return; // CRATONVM_PROXY_USER_SUBCLASS_ORDINARY=0 in this environment
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let Some((shim, proxy)) = fixture(&shared) else {
        return;
    };
    // Fresh `String`s: pointers no compiled site interned, so the check
    // takes the by-name tail.
    let iface = String::from(IFACE);
    let other = String::from(OTHER);
    let mut obj = proxy;
    let mut stable_negative = false;
    let mut instance_admitted = false;
    // SAFETY: `obj` is a live object of `shared`'s own heap and no
    // compiled code or JIT thread is involved.
    let admitted = unsafe {
        jit_typecheck_resolve(
            &shared,
            shim,
            &mut obj,
            &iface,
            false,
            &mut stable_negative,
            &mut instance_admitted,
        )
    };
    assert!(admitted, "Proxy$Instance for {{IFACE}} instanceof IFACE");
    assert!(
        instance_admitted,
        "a slot-1 answer must not be memoized per class id"
    );
    let mut instance_admitted = false;
    // SAFETY: as above.
    let refused = !unsafe {
        jit_typecheck_resolve(
            &shared,
            shim,
            &mut obj,
            &other,
            false,
            &mut stable_negative,
            &mut instance_admitted,
        )
    };
    assert!(refused, "Proxy$Instance for {{IFACE}} is not an OTHER");
    // Every such proxy shares the shim's id: its refusal is not a fact
    // about the class, in either tier's negative memo.
    assert!(
        !crate::runtime::interpreter::receiver_class_determines_cast_verdict(&shared, shim)
    );
}
