//! Round 15 wave 5, lane classsite
//! (`r15w5-fieldowner2-isolated-loader-class-sites-reask-the-loader-CLOSED-20260930.md`):
//! the per-thread class-site tables admit a loader-namespaced class's
//! delegated answer once it is the `CONSTANT_Class` entry's recorded
//! resolution, and only for that entry and that answer.
use super::site_fill_admitted;
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::runtime::resolve::MemberResolver;
use crate::threading::jvm_thread::{JvmThread, ThreadId};
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// Define and register a field-less, method-less class (the
/// `constants.rs` `i9_l2_class_access_tests` helper).
fn define(shared: &SharedVm, name: &str, loader: ClassLoaderId) -> ClassId {
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
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(loader, name, id);
    id
}

#[test]
fn a_recorded_class_entry_is_admitted_for_its_entry_and_answer_only() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let mut thread = JvmThread::new(ThreadId(0), "test");
    let user = ClassLoaderId::UserDefined(4151);
    let referencing = define(&shared, "cs15/Reader", user);
    let delegated = define(&shared, "cs15/Shared", ClassLoaderId::Application);
    let other = define(&shared, "cs15/Other", ClassLoaderId::Application);

    // Another loader's class, with no record: refused, as before.
    assert!(!site_fill_admitted(
        &shared,
        &mut thread,
        (referencing, 7),
        "cs15/Shared",
        delegated
    ));

    let resolver = MemberResolver::new(&shared);
    let as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let Some(recorded) =
        resolver.record_class_entry_as_of(resolver.scope(referencing), 7, delegated, as_of)
    else {
        // A redefinition elsewhere in the process straddled the fill
        // (`fill_is_current` cannot always tell VMs apart): nothing to test.
        return;
    };
    assert_eq!(recorded, delegated);
    assert_eq!(
        resolver.record_class_entry_as_of(resolver.scope(referencing), 7, other, as_of),
        Some(delegated),
        "the entry keeps its first resolution"
    );

    assert!(site_fill_admitted(
        &shared,
        &mut thread,
        (referencing, 7),
        "cs15/Shared",
        delegated
    ));
    assert!(
        !site_fill_admitted(&shared, &mut thread, (referencing, 7), "cs15/Shared", other),
        "an answer that is not the entry's"
    );
    assert!(
        !site_fill_admitted(
            &shared,
            &mut thread,
            (referencing, 8),
            "cs15/Shared",
            delegated
        ),
        "another entry of the same class"
    );
}
