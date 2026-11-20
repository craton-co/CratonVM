//! Round 15 wave 5, lane classsite2: a `multianewarray` entry's recorded
//! resolution answers every level's component off its `array_info`
//! chain, without resolving a level by name, and a chain link without
//! `array_info` hands the site back to the by-name loop.
use super::multianewarray_entry_components;
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::runtime::resolve::MemberResolver;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// A field-less, method-less class (the `opcodes.rs`
/// `r15w5_classsite_tests` helper), with `array_info` over `component`
/// when one is given.
fn define(
    shared: &SharedVm,
    name: &str,
    loader: ClassLoaderId,
    component: Option<(ClassId, u8)>,
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
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
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
        origin: cratonvm_classloading::ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: component.map(|(component_class_id, array_dimension)| {
            cratonvm_classloading::ArrayInfo {
                component_class_id,
                array_dimension,
                leaf_component_name: cratonvm_types::intern_arc("cs2/MA"),
            }
        }),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(loader, name, id);
    id
}

#[test]
fn a_recorded_entry_answers_every_level_off_its_chain() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let app = ClassLoaderId::Application;
    let referencing = define(
        &shared,
        "cs2/Reader",
        ClassLoaderId::UserDefined(4152),
        None,
    );
    let element = define(&shared, "cs2/MA", app, None);
    let one = define(&shared, "[Lcs2/MA;", app, Some((element, 1)));
    let two = define(&shared, "[[Lcs2/MA;", app, Some((one, 2)));

    let resolver = MemberResolver::new(&shared);
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let recorded = (
        resolver.record_class_entry_as_of(resolver.scope(referencing), 9, two, as_of),
        resolver.record_class_entry_as_of(resolver.scope(referencing), 10, element, as_of),
    );
    let (Some(_), Some(_)) = recorded else {
        // A redefinition elsewhere in the process straddled the fill
        // (`fill_is_current` cannot always tell VMs apart): nothing to test.
        return;
    };

    let walk = |thread: &mut JvmThread, cp_index: u16, name: &str, levels: usize| {
        multianewarray_entry_components(
            &shared,
            thread,
            (referencing, cp_index),
            name,
            (levels, true),
            as_of,
        )
        .ok()
        .flatten()
    };
    assert_eq!(
        walk(&mut thread, 9, "[[Lcs2/MA;", 2),
        Some(vec![one, element]),
        "the outer level's component, then the element"
    );
    assert_eq!(
        walk(&mut thread, 9, "[[Lcs2/MA;", 1),
        Some(vec![one]),
        "fewer dimensions than brackets"
    );
    assert_eq!(
        walk(&mut thread, 10, "[[Lcs2/MA;", 1),
        None,
        "a link without `array_info`: the by-name loop decides"
    );
}

/// JIT round 16, lane classsite (R3): the walk taken for a class whose
/// entries keep no record (`record == false`) files none, whatever its
/// resolution answers.
#[test]
fn an_unrecorded_walk_files_no_record() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let app = ClassLoaderId::Application;
    let referencing = define(&shared, "cs2/PlainReader", app, None);
    let element = define(&shared, "cs2/MA", app, None);
    let one = define(&shared, "[Lcs2/MA;", app, Some((element, 1)));
    let _two = define(&shared, "[[Lcs2/MA;", app, Some((one, 2)));
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let _ = multianewarray_entry_components(
        &shared,
        &mut thread,
        (referencing, 11),
        "[[Lcs2/MA;",
        (2, false),
        as_of,
    );
    let resolver = MemberResolver::new(&shared);
    assert_eq!(
        resolver.probe_class_entry(resolver.scope(referencing), 11),
        None,
        "the record stays an isolated loader's"
    );
}
