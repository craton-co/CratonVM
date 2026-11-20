use super::{
    jit_field_owner_refusal, CpResolvers, JitFieldSiteMemo, ResolvedField, SubstitutedOwners,
};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// A loader namespace id no allocator hands out in a unit-test process.
const ISOLATED_NS: u32 = 0x7f15_0401;
const SHARED: &str = "cratonvm/r15w4/Shared";

/// Define and register a field-less, method-less class of `loader`.
fn define(
    shared: &SharedVm,
    name: &str,
    loader: ClassLoaderId,
    pool: Vec<ConstantPoolEntry>,
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
        constant_pool: ConstantPool::new(pool),
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

/// A pool whose entry 6 is `Fieldref SHARED.marker:I`.
fn pool_naming_shared() -> Vec<ConstantPoolEntry> {
    vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(SHARED.into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::Utf8("marker".into()),
        ConstantPoolEntry::Utf8("I".into()),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        ConstantPoolEntry::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        },
    ]
}

/// An isolated-shaped loader (delegation chain recorded as ending at the
/// platform loader) that has not defined the owner yet: the interpreter
/// drives it, so a threadless answer naming the application's copy (the
/// flat store's, or a stale initiating memo's) is refused. The same pool
/// entry of an application-loader class is the flat store's own answer.
#[test]
fn an_isolated_loaders_site_bound_to_the_application_copy_is_refused() {
    let shared = SharedVm::new(VmConfig::default());
    shared.loader_identity.user_loader_parents().register(
        ISOLATED_NS,
        ClassLoaderId::NATIVE_EXTENSION,
    );
    let app_copy = define(
        &shared,
        SHARED,
        ClassLoaderId::Application,
        vec![ConstantPoolEntry::Tombstone],
    );
    let iso_user = define(
        &shared,
        "cratonvm/r15w4/IsoUser",
        ClassLoaderId::UserDefined(ISOLATED_NS),
        pool_naming_shared(),
    );
    let app_user = define(
        &shared,
        "cratonvm/r15w4/AppUser",
        ClassLoaderId::Application,
        pool_naming_shared(),
    );
    let field = ResolvedField {
        declaring_class_id: app_copy,
        field_index: 0,
        is_static: true,
        is_volatile: false,
        is_final: false,
        is_reference: false,
        desc_byte: b'I',
    };
    assert_eq!(
        jit_field_owner_refusal(&shared, iso_user, 6, &field),
        Some("field-owner-isolated-app-copy")
    );
    assert_eq!(jit_field_owner_refusal(&shared, app_user, 6, &field), None);
}

/// CH3-1: the first ask's answer (here: no resolution, the class does not
/// exist) is the one every later ask of the same entry gets.
#[test]
fn every_field_ask_shares_one_answer_per_site() {
    let shared = SharedVm::new(VmConfig::default());
    let cp = CpResolvers {
        shared: &shared,
        class_id: ClassId::new(0x7fff_fff0),
        substituted_owners: SubstitutedOwners::default(),
        method_name: "run",
        method_descriptor: "()V",
        code: &[],
        ldc2w_trace: None,
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    };
    let memo = JitFieldSiteMemo::default();
    assert!(cp.resolved_field(Some(&memo), 6).is_none());
    assert!(matches!(memo.rows.borrow().get(&6), Some(None)));
    assert!(cp.instance_field(Some(&memo), 6).is_none());
    assert!(cp.static_field(Some(&memo), 6).is_none());
    assert!(!cp.static_field_is_trusted_final(Some(&memo), 6));
    assert!(!cp.field_is_volatile(Some(&memo), 6));
    assert!(cp.instance_field_declaring_class(Some(&memo), 6).is_none());
    assert_eq!(memo.rows.borrow().len(), 1);
}
