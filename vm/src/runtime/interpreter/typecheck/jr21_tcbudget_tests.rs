use super::{synthetic_implements, synthetic_implements_by_name};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::jit::helpers::{supertype_closure_is_hierarchy_only, vh_const_display_facts};
use crate::vm::SharedVm;
use cratonvm_classloading::ClassOrigin;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use std::sync::Arc;

const LEVELS: usize = 10;

fn define(
    shared: &SharedVm,
    name: &str,
    superclass: Option<ClassId>,
    interfaces: Vec<ClassId>,
    access_flags: ClassAccessFlags,
    origin: ClassOrigin,
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
        interfaces,
        fields: vec![],
        methods: vec![],
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
    id
}

fn real() -> ClassOrigin {
    ClassOrigin::ApplicationClassPath {
        source: Arc::from("file:/jr21tcbudget/"),
    }
}

/// A user class `cratonvm/jr21tc/Outer$Itr` (a name the `Iterator` rule
/// matches) over a two-wide diamond lattice of `2 * LEVELS` interfaces:
/// both interfaces of level `k` extend both of level `k - 1`. Not an
/// `Iterator`. `stub_at_bottom` makes one bottom interface a fabricated
/// stub (the negative control: the rules must stay on).
fn lattice(shared: &SharedVm, tag: &str, stub_at_bottom: bool) -> ClassId {
    let iface =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
    let base = define(
        shared,
        &format!("cratonvm/jr21tc/{tag}Base"),
        None,
        vec![],
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        real(),
    );
    let mut prev: Vec<ClassId> = Vec::new();
    for level in 0..LEVELS {
        let mut row = Vec::new();
        for side in ["A", "B"] {
            let origin = if stub_at_bottom && level == 0 && side == "A" {
                ClassOrigin::CompatibilityStub {
                    reason: Arc::from("jr21 tcbudget negative control"),
                }
            } else {
                real()
            };
            row.push(define(
                shared,
                &format!("cratonvm/jr21tc/{tag}I{level}{side}"),
                None,
                prev.clone(),
                iface,
                origin,
            ));
        }
        prev = row;
    }
    define(
        shared,
        &format!("cratonvm/jr21tc/{tag}Outer$Itr"),
        Some(base),
        prev,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        real(),
    )
}

/// Visits of the walk this lane replaced: once per PATH, no visited set.
fn tree_visits(shared: &SharedVm, id: ClassId) -> u64 {
    let cm = shared.classes.class_manager.read();
    fn go(cm: &crate::classloading::ClassManager, id: ClassId) -> u64 {
        let Some(c) = cm.get_class(id) else {
            return 1;
        };
        let mut n = 1;
        for &i in c.interfaces.iter() {
            n += go(cm, i);
        }
        if let Some(s) = c.superclass {
            n += go(cm, s);
        }
        n
    }
    go(&cm, id)
}

#[test]
fn a_diamond_heavy_real_closure_is_hierarchy_only_and_the_name_rule_stays_off() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let itr = lattice(&shared, "Real", false);
    // The fixture is what broke the old walk: far past its 256 visits.
    assert!(tree_visits(&shared, itr) > 256);
    // Positive control: the name alone WOULD be admitted.
    assert!(synthetic_implements_by_name(
        "cratonvm/jr21tc/RealOuter$Itr",
        "java/util/Iterator"
    ));
    {
        let cm = shared.classes.class_manager.read();
        assert!(supertype_closure_is_hierarchy_only(&cm, itr));
        let (chain, interfaces, verdict) =
            vh_const_display_facts(&cm, itr).expect("a class display");
        assert_eq!(chain.len(), 2);
        assert_eq!(interfaces.len(), 2 * LEVELS);
        assert!(verdict, "display verdict and the interpreter's rule agree");
    }
    // HotSpot: `new Outer.Itr() instanceof Iterator` is false.
    assert!(!synthetic_implements(&shared, itr, "java/util/Iterator"));
}

#[test]
fn a_stub_deep_in_a_diamond_closure_keeps_the_rules_at_every_door() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let itr = lattice(&shared, "Stub", true);
    {
        let cm = shared.classes.class_manager.read();
        assert!(!supertype_closure_is_hierarchy_only(&cm, itr));
        let (_, _, verdict) = vh_const_display_facts(&cm, itr).expect("a class display");
        assert!(!verdict, "display verdict and the interpreter's rule agree");
    }
    // A fabricated supertype may lack its real edges: the by-name rule
    // is kept (and a compiled site declines its miss to the helper).
    assert!(synthetic_implements(&shared, itr, "java/util/Iterator"));
}

/// A lambda class carries its host's name; the `Iterator` rule must not
/// read `Outer$Itr` out of `Outer$Itr$$Lambda/0x...` (HotSpot: false).
#[test]
fn a_lambda_hosted_in_an_itr_class_is_not_an_iterator_by_name() {
    for name in [
        "cratonvm/jr21tc/Outer$Itr$$Lambda/0x0000000080000012",
        "cratonvm/jr21tc/Outer$Itr$$Lambda",
        "java/util/ArrayList$$Lambda/0x0000000000000400",
    ] {
        for target in [
            "java/util/Iterator",
            "java/util/List",
            "java/util/Collection",
            "java/lang/Iterable",
            "java/io/Serializable",
            "java/lang/Comparable",
        ] {
            assert!(
                !synthetic_implements_by_name(name, target),
                "{name} admitted for {target} by name"
            );
        }
        assert!(synthetic_implements_by_name(name, "java/lang/Object"));
    }
    // The rule itself still serves its own population.
    assert!(synthetic_implements_by_name(
        "java/util/ArrayList$Itr",
        "java/util/Iterator"
    ));
}

#[test]
fn a_receiver_past_the_class_band_is_never_hierarchy_only() {
    let shared = Arc::new(SharedVm::new(crate::config::VmConfig::default()));
    let cm = shared.classes.class_manager.read();
    assert!(!supertype_closure_is_hierarchy_only(
        &cm,
        ClassId::new(0x8000_0001)
    ));
}
