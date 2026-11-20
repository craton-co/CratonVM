use super::hotspot_class_modifiers;
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState, ClassStore};
use cratonvm_classloading::{ArrayInfo, InnerClassEntry};
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use std::sync::Arc;

fn add(
    store: &mut ClassStore,
    name: &str,
    access_flags: ClassAccessFlags,
    inner_classes: Vec<InnerClassEntry>,
    array_info: Option<ArrayInfo>,
) -> ClassId {
    let id = store.next_id();
    store.add(Class {
        id,
        loader_id: ClassLoaderId::Bootstrap,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags,
        superclass: None,
        interfaces: Vec::new(),
        fields: Vec::new(),
        methods: Vec::new(),
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: Vec::new(),
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes,
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info,
        init_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    })
}

fn array_of(component: ClassId, dimension: u8, leaf: &str) -> Option<ArrayInfo> {
    Some(ArrayInfo {
        component_class_id: component,
        array_dimension: dimension,
        leaf_component_name: Arc::from(leaf),
    })
}

/// HotSpot's `modifier_flags` (`L3W52ClassMirrorModifiers`): no
/// `ACC_SUPER`; a member class's InnerClasses flags; an array's
/// `FINAL | ABSTRACT` plus its leaf element's access bits.
#[test]
fn the_mirror_carries_hotspots_modifiers() {
    let mut store = ClassStore::new();
    let object = add(
        &mut store,
        "java/lang/Object",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        Vec::new(),
        None,
    );
    // `private static class P` inside `O`: the class file says
    // package-private `SUPER`, its InnerClasses entry `PRIVATE | STATIC`.
    let nested = add(
        &mut store,
        "O$P",
        ClassAccessFlags::SUPER,
        vec![InnerClassEntry {
            inner_class: "O$P".into(),
            outer_class: "O".into(),
            inner_name: "P".into(),
            access_flags: 0x000a,
        }],
        None,
    );
    let nested_array = add(
        &mut store,
        "[LO$P;",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
        Vec::new(),
        array_of(nested, 1, "O$P"),
    );
    let nested_array2 = add(
        &mut store,
        "[[LO$P;",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
        Vec::new(),
        array_of(nested_array, 2, "O$P"),
    );
    let int_array = add(
        &mut store,
        "[I",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
        Vec::new(),
        None,
    );
    let int_array2 = add(
        &mut store,
        "[[I",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
        Vec::new(),
        array_of(int_array, 2, "I"),
    );
    assert_eq!(hotspot_class_modifiers(&store, object, |_| false), 0x0001);
    assert_eq!(hotspot_class_modifiers(&store, nested, |_| false), 0x000a);
    assert_eq!(hotspot_class_modifiers(&store, nested_array, |_| false), 0x0412);
    assert_eq!(hotspot_class_modifiers(&store, nested_array2, |_| false), 0x0412);
    assert_eq!(hotspot_class_modifiers(&store, int_array2, |_| false), 0x0411);
    // Not in the store (a lambda proxy is answered before this): 0.
    assert_eq!(
        hotspot_class_modifiers(&store, ClassId::new(0x8000_0001), |_| false),
        0
    );
    // An array of a lambda proxy (the leaf is not in the store): no
    // access bit, `final synthetic` on HotSpot, so 0x410; an unknown
    // leaf that is not a proxy keeps the old public default.
    let proxy = ClassId::new(0x8000_0002);
    let proxy_array = add(
        &mut store,
        "[LO$$Lambda/0x01;",
        ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
        Vec::new(),
        array_of(proxy, 1, "O$$Lambda/0x01"),
    );
    assert_eq!(
        hotspot_class_modifiers(&store, proxy_array, |id| id == proxy),
        0x0410
    );
    assert_eq!(hotspot_class_modifiers(&store, proxy_array, |_| false), 0x0411);
}
