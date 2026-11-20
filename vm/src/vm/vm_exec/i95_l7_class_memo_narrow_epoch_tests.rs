use super::*;
use crate::config::VmConfig;
use std::sync::Arc;

fn narrow_shared() -> Arc<SharedVm> {
    let mut config = VmConfig::default();
    config.class_memo_narrow_epoch = true;
    Arc::new(SharedVm::new(config))
}

/// A real class with one instance field of each descriptor, no
/// superclass, registered under `name` (the shape of the module tests'
/// `add_real_class_with_field_descriptors`).
fn add_class(shared: &SharedVm, name: &str, descriptors: &[&str]) -> ClassId {
    use cratonvm_classloading::{Class, ClassLoaderId, ClassState};
    use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use cratonvm_reader::field::ClassFileField;

    let fields: Vec<ClassFileField> = descriptors
        .iter()
        .enumerate()
        .map(|(i, d)| ClassFileField {
            access_flags: FieldAccessFlags::PRIVATE,
            name: Arc::from(format!("f{i}")),
            descriptor: Arc::from(*d),
            attributes: Vec::new(),
        })
        .collect();
    let num_fields = fields.len();
    let mut cm = shared.classes.class_manager_write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Initialized,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields,
        methods: vec![],
        first_field_index: 0,
        num_total_fields: num_fields,
        bootstrap_methods: vec![],
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
        signature: None,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

/// The narrow stamp does not move on a definition of an unrelated class
/// (`class_origin_epoch` does). Both counters are process-shared with the
/// tests running beside this one (the edge epoch per store slot, the
/// in-place epoch in every slot), so a window spoiled by another test's
/// write is retried; if the definition itself moved the stamp, every
/// window would be dirty and this would fail.
#[test]
fn an_unrelated_definition_leaves_the_narrow_stamp() {
    let shared = narrow_shared();
    add_class(&shared, "i95l7/Base", &["I"]);
    const ATTEMPTS: usize = 64;
    let mut quiet = false;
    for i in 0..ATTEMPTS {
        let before = class_id_memo_epoch(&shared);
        let wide_before = cratonvm_classloading::class_origin_epoch();
        add_class(&shared, &format!("i95l7/Noise{i}"), &["J"]);
        assert!(
            cratonvm_classloading::class_origin_epoch() > wide_before,
            "a definition moves the wide stamp, or this test proves nothing"
        );
        if class_id_memo_epoch(&shared) == before {
            quiet = true;
            break;
        }
    }
    assert!(quiet, "no quiet window in {ATTEMPTS} definitions");
}

/// A transient answer (the receiver is a stub) memoised under the narrow
/// stamp is retired by the stub's promotion (`set_origin`, which moves
/// `class_origin_in_place_epoch`).
#[test]
fn a_promotion_retires_a_transient_answer_under_the_narrow_stamp() {
    let shared = narrow_shared();
    let cid = add_class(&shared, "i95l7/StubPromote", &["J"]);
    {
        let mut cm = shared.classes.class_manager_write();
        let cls = cm.class_store.get_mut(cid).expect("class present");
        cls.set_origin(cratonvm_classloading::ClassOrigin::compatibility_stub("i95l7"));
    }
    assert_eq!(resolve_field_descriptor_byte_cached(&shared, cid, 0), None);
    {
        let mut cm = shared.classes.class_manager_write();
        let cls = cm.class_store.get_mut(cid).expect("class present");
        cls.set_origin(cratonvm_classloading::ClassOrigin::default());
    }
    assert_eq!(
        resolve_field_descriptor_byte_cached(&shared, cid, 0),
        Some(b'J'),
        "the promoted class's real descriptor, not the stub's memoised miss"
    );
}

/// A positive by-name answer survives definitions under the narrow stamp
/// and answers what the uncached walk answers.
#[test]
fn a_by_name_answer_is_the_walks_answer() {
    let shared = narrow_shared();
    let cid = add_class(&shared, "i95l7/ByName", &["I", "J"]);
    assert_eq!(resolve_field_slot_by_name_cached(&shared, cid, "f1"), Some(1));
    add_class(&shared, "i95l7/ByNameNoise", &["I"]);
    assert_eq!(resolve_field_slot_by_name_cached(&shared, cid, "f1"), Some(1));
    assert_eq!(resolve_field_slot_by_name_cached(&shared, cid, "f9"), None);
}
