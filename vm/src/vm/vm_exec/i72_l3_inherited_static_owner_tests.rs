use super::{inherited_static_declarer_initialized, static_owner_init_pending, SharedVm};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;
use std::sync::Arc;

/// A `--compatible` VM with the switch set to `on`, whatever the
/// environment says.
fn vm(on: bool) -> Arc<SharedVm> {
    let mut config = crate::config::VmConfig::default();
    config.static_owner_supers_initialized = on;
    Arc::new(SharedVm::new(config))
}

/// Define `name` (superclass `superclass`) declaring `methods`
/// (`(name, is_static)`, all `()V`), initialized when `initialized`.
fn add_class(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    interface: bool,
    methods: &[(&str, bool)],
    initialized: bool,
) -> ClassId {
    let methods = methods
        .iter()
        .map(|&(method, is_static)| ClassFileMethod {
            name: Arc::from(method),
            descriptor: Arc::from("()V"),
            access_flags: if is_static {
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC
            } else {
                MethodAccessFlags::PUBLIC
            },
            attributes: vec![],
        })
        .collect();
    let access_flags = if interface {
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT
    } else {
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER
    };
    let init_state = if initialized {
        cratonvm_classloading::CLASS_INIT_INITIALIZED
    } else {
        0
    };
    let mut cm = shared.classes.class_manager.write();
    let id = cm.class_store.next_id();
    cm.class_store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: if initialized {
            ClassState::Initialized
        } else {
            ClassState::Loaded
        },
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags,
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
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(init_state)),
    });
    cm.register_class_name(ClassLoaderId::Application, name, id);
    id
}

/// `Base` (declares `static helper()` and `instance()`, initialized when
/// `base_initialized`) <- `Mid` <- `Sub` (neither initialized; `Sub`
/// declares `static own()`), and an interface `Iface` declaring
/// `static helper()`.
struct Hierarchy {
    base: ClassId,
    sub: ClassId,
    iface: ClassId,
}

fn hierarchy(shared: &SharedVm, base_initialized: bool) -> Hierarchy {
    let base = add_class(
        shared,
        "cratonvm/i72l3/Base",
        None,
        false,
        &[("helper", true), ("instance", false)],
        base_initialized,
    );
    let mid = add_class(shared, "cratonvm/i72l3/Mid", Some(base), false, &[], false);
    let sub = add_class(
        shared,
        "cratonvm/i72l3/Sub",
        Some(mid),
        false,
        &[("own", true)],
        false,
    );
    let iface = add_class(
        shared,
        "cratonvm/i72l3/Iface",
        None,
        true,
        &[("helper", true)],
        false,
    );
    Hierarchy { base, sub, iface }
}

/// The walk answers `true` for an inherited static of an initialized
/// superclass two levels up; positive control: the same shape with the
/// declarer not initialized answers `false`.
#[test]
fn the_walk_answers_an_inherited_static_of_an_initialized_declarer() {
    for base_initialized in [true, false] {
        let shared = vm(true);
        let h = hierarchy(&shared, base_initialized);
        assert_eq!(
            inherited_static_declarer_initialized(&shared, h.sub, "helper", "()V"),
            base_initialized,
            "base_initialized={base_initialized}"
        );
    }
}

/// Every other shape is left to the resolution: the owner declares the
/// static, the first match is an instance method, nothing on the chain
/// declares it, a different descriptor, an interface owner.
#[test]
fn the_walk_leaves_every_other_shape_to_the_resolution() {
    let shared = vm(true);
    let h = hierarchy(&shared, true);
    for (owner, name, descriptor, why) in [
        (h.sub, "own", "()V", "the owner declares it"),
        (h.sub, "instance", "()V", "an instance method"),
        (h.sub, "missing", "()V", "no declaration on the chain"),
        (h.sub, "helper", "(I)V", "another descriptor"),
        (h.iface, "helper", "()V", "an interface owner"),
    ] {
        assert!(
            !inherited_static_declarer_initialized(&shared, owner, name, descriptor),
            "{why}"
        );
    }
}

/// The switch never changes `static_owner_init_pending`'s answer, on or
/// off: `None` for an inherited static of an initialized declarer, the
/// declarer when it is not initialized, the owner when it declares the
/// static itself.
#[test]
fn the_switch_keeps_the_resolutions_answers() {
    const SUB: &str = "cratonvm/i72l3/Sub";
    const BASE: &str = "cratonvm/i72l3/Base";
    for on in [false, true] {
        for base_initialized in [true, false] {
            let shared = vm(on);
            let h = hierarchy(&shared, base_initialized);
            assert_eq!(
                static_owner_init_pending(&shared, h.sub, SUB, "helper", "()V"),
                (!base_initialized).then_some(h.base),
                "on={on} base_initialized={base_initialized}: inherited"
            );
            assert_eq!(
                static_owner_init_pending(&shared, h.sub, SUB, "own", "()V"),
                Some(h.sub),
                "on={on}: the owner declares it"
            );
            assert_eq!(
                static_owner_init_pending(&shared, h.base, BASE, "helper", "()V"),
                (!base_initialized).then_some(h.base),
                "on={on}: the declarer itself"
            );
        }
    }
}
