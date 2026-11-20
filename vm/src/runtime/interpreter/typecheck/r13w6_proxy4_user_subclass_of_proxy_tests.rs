use super::{
    aastore_element_assignable, class_is_generated_proxy_or_shim,
    name_has_generated_proxy_shape, proxy_instance_satisfies_resolved,
    proxy_instance_satisfies_target,
};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::vm::SharedVm;
use cratonvm_classloading::ClassOrigin;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use std::sync::Arc;

const IFACE: &str = "cratonvm/r13p4/Iface";

fn define(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    access_flags: ClassAccessFlags,
    num_total_fields: usize,
    origin: ClassOrigin,
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
        superclass,
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
        origin,
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

struct Fixture {
    iface: ClassId,
    user: ClassId,
    generated: ClassId,
    shim: ClassId,
    /// An instance of the user subclass whose slot 1 holds `Class[]{Iface}`.
    user_obj: ObjectRef,
    /// An instance of a generated `$ProxyN` recorded for `Iface`.
    generated_obj: ObjectRef,
    /// `Iface[]`.
    iface_arr: ObjectRef,
}

fn fixture(shared: &SharedVm) -> Option<Fixture> {
    let plain = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let iface = define(
        shared,
        IFACE,
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT,
        0,
        ClassOrigin::default(),
        true,
    );
    let proxy_base = define(
        shared,
        "java/lang/reflect/Proxy",
        None,
        plain,
        1,
        ClassOrigin::default(),
        false,
    );
    let user = define(
        shared,
        "cratonvm/r13p4/Base2",
        Some(proxy_base),
        plain,
        2,
        ClassOrigin::ApplicationClassPath {
            source: Arc::from("file:/r13p4/"),
        },
        true,
    );
    let generated = define(
        shared,
        "jdk/proxy4/$Proxy4",
        Some(proxy_base),
        plain | ClassAccessFlags::FINAL,
        1,
        ClassOrigin::GeneratedProxy {
            interfaces: Arc::from(vec![iface]),
        },
        false,
    );
    let shim = define(
        shared,
        "java/lang/reflect/Proxy$Instance",
        None,
        plain,
        3,
        ClassOrigin::VmInternal,
        false,
    );
    let mirror = shared.mem.heap.alloc_object(proxy_base, 1);
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .insert(mirror, iface);
    let class_array =
        shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, 1);
    if shared
        .mem
        .heap
        .set_array_element(class_array, 0, Value::Object(Some(mirror)))
        .is_err()
    {
        return None;
    }
    let user_obj = shared.mem.heap.alloc_object(user, 2);
    let generated_obj = shared.mem.heap.alloc_object(generated, 1);
    if shared.mem.heap.class_id_of(user_obj) != user
        || shared.mem.heap.class_id_of(generated_obj) != generated
    {
        return None; // This heap cannot mint the fixture's objects.
    }
    shared
        .mem
        .heap
        .set_field(user_obj, 1, Value::Object(Some(class_array)));
    let iface_arr = shared
        .mem
        .heap
        .alloc_array(iface, ArrayElementType::Reference, 1);
    Some(Fixture {
        iface,
        user,
        generated,
        shim,
        user_obj,
        generated_obj,
        iface_arr,
    })
}

/// Patch (a): slot 1 of a user subclass is the user's field, never the
/// proxy's interface list. Holds with the switch off too.
#[test]
fn a_user_subclass_field_is_not_read_as_the_interface_list() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let Some(f) = fixture(&shared) else {
        return;
    };
    assert!(
        !proxy_instance_satisfies_resolved(&shared, f.user_obj, IFACE, f.iface),
        "Base2 implements nothing; HotSpot answers `instanceof Iface` false"
    );
    assert!(!proxy_instance_satisfies_target(&shared, f.user_obj, IFACE));
}

/// Patch (b): the precise predicate, and `aastore` no longer fails open for
/// the user subclass while the generated proxy keeps its recorded set.
#[test]
fn a_user_subclass_is_not_a_proxy_to_aastore() {
    if !cratonvm_native_builtins::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return;
    }
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let Some(f) = fixture(&shared) else {
        return;
    };
    assert!(!class_is_generated_proxy_or_shim(&shared, f.user));
    assert!(class_is_generated_proxy_or_shim(&shared, f.generated));
    assert!(class_is_generated_proxy_or_shim(&shared, f.shim));
    assert!(
        !aastore_element_assignable(&shared, f.iface_arr, f.user_obj),
        "`Iface[] <- new Base2(h)` is an ArrayStoreException on HotSpot"
    );
    assert!(
        aastore_element_assignable(&shared, f.iface_arr, f.generated_obj),
        "the control: a proxy generated for Iface stores into Iface[]"
    );
}

/// The `$Proxy` name arm of `aastore` matches the generated shape only.
#[test]
fn the_proxy_name_arm_matches_the_generated_shape_only() {
    if !cratonvm_native_builtins::reflect_annotations::proxy_user_subclass_is_ordinary() {
        return;
    }
    for name in ["jdk/proxy3/$Proxy27", "com/sun/proxy/$Proxy0", "$Proxy12"] {
        assert!(name_has_generated_proxy_shape(name), "{name}");
    }
    for name in [
        "com/example/Settings$ProxyConfig",
        "java/lang/reflect/Proxy$ProxyBuilder",
        "jdk/proxy1/$Proxy",
        "jdk/proxy1/$Proxy0$Inner",
    ] {
        assert!(!name_has_generated_proxy_shape(name), "{name}");
    }
}
