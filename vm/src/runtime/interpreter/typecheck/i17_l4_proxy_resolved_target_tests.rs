use super::{proxy_instance_satisfies_resolved, proxy_instance_satisfies_target};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::runtime::proxy::PROXY_FIELD_INTERFACES;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use std::sync::Arc;

const DUP: &str = "cratonvm/i17l4/Dup";

/// A field-less, method-less class; registered under `loader` only when
/// `register` is set.
fn define(
    shared: &SharedVm,
    name: &str,
    loader: ClassLoaderId,
    access_flags: ClassAccessFlags,
    num_total_fields: usize,
    register: bool,
) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: loader,
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
        cm.register_class_name(loader, name, id);
    }
    id
}

/// Two interfaces named `DUP` (application loader: `a`, a user loader:
/// `b`) and a synthetic-shape `Proxy$Instance` created for `b`.
fn fixture(shared: &SharedVm) -> Option<(ClassId, ClassId, ObjectRef)> {
    let iface =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
    let a = define(shared, DUP, ClassLoaderId::Application, iface, 0, true);
    let b = define(shared, DUP, ClassLoaderId::UserDefined(7), iface, 0, true);
    let proxy_class = define(
        shared,
        "java/lang/reflect/Proxy$Instance",
        ClassLoaderId::Bootstrap,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        3,
        false,
    );
    let mirror = shared.mem.heap.alloc_object(proxy_class, 0);
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .insert(mirror, b);
    let interfaces =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    if shared
        .mem
        .heap
        .set_array_element(interfaces, 0, Value::Object(Some(mirror)))
        .is_err()
    {
        return None;
    }
    let proxy = shared.mem.heap.alloc_object(proxy_class, 3);
    if shared.mem.heap.class_id_of(proxy) != proxy_class {
        return None; // This heap cannot mint the fixture's objects.
    }
    shared.mem.heap.set_field(
        proxy,
        PROXY_FIELD_INTERFACES,
        Value::Object(Some(interfaces)),
    );
    Some((a, b, proxy))
}

/// The resolved class the proxy implements is admitted by identity, and
/// the by-name form keeps answering for the callers with no id.
#[test]
fn the_resolved_target_is_compared_by_identity() {
    // `--compatible` with the `NamespacedClassTypes` row OFF (its default
    // until interpreter round i1 wave 52): the by-name rule this test was
    // written for.
    let mut vm = SharedVm::new(crate::config::VmConfig::default());
    vm.classes.hotspot_aligned = vm.classes.hotspot_aligned.with(
        cratonvm_types::HotSpotAlignment::NamespacedClassTypes,
        false,
    );
    let shared = Arc::new(vm);
    let Some((a, b, proxy)) = fixture(&shared) else {
        return;
    };
    assert!(proxy_instance_satisfies_resolved(&shared, proxy, DUP, b));
    // The row off keeps the same-name admission for the other loader's
    // copy (the opcodes' `loader_aware_name_assignable` agrees).
    assert!(proxy_instance_satisfies_resolved(&shared, proxy, DUP, a));
    assert!(proxy_instance_satisfies_target(&shared, proxy, DUP));
    assert!(proxy_instance_satisfies_resolved(
        &shared,
        proxy,
        "java/io/Serializable",
        a
    ));
}

/// `--compatible` with its default alignment rows (the
/// `NamespacedClassTypes` row on since wave 52, lane L4) answers as
/// `--real-jdk` does: the other loader's same-named interface is refused.
#[test]
fn under_compatible_with_the_namespaced_row_the_other_loaders_copy_is_refused() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let Some((a, b, proxy)) = fixture(&shared) else {
        return;
    };
    assert!(proxy_instance_satisfies_resolved(&shared, proxy, DUP, b));
    assert!(!proxy_instance_satisfies_resolved(&shared, proxy, DUP, a));
}

/// Under `--real-jdk` the other loader's same-named interface is a
/// different class: the resolved form refuses it.
#[test]
fn under_jdk_only_the_other_loaders_copy_is_refused() {
    let mut config = crate::config::VmConfig::default();
    config.compatibility_mode = cratonvm_types::compat::CompatibilityMode::JdkOnly;
    // `JdkOnly` with the synthetic library is a rejected pair.
    config.use_synthetic_jdk = false;
    let shared = Arc::new(SharedVm::new(config));
    let Some((a, b, proxy)) = fixture(&shared) else {
        return;
    };
    assert!(proxy_instance_satisfies_resolved(&shared, proxy, DUP, b));
    assert!(!proxy_instance_satisfies_resolved(&shared, proxy, DUP, a));
}

/// Structural: the three opcode doors (`checkcast`, `instanceof`'s shared
/// predicate, the negative-memo cross-check) pass their resolved id.
#[test]
fn the_opcode_doors_pass_the_resolved_target() {
    let src = include_str!("../opcodes.rs").replace("\r\n", "\n");
    let production = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
    assert_eq!(
        production
            .matches("proxy_instance_satisfies_resolved(")
            .count(),
        3
    );
    assert_eq!(
        production
            .matches("proxy_instance_satisfies_target(")
            .count(),
        0
    );
}
