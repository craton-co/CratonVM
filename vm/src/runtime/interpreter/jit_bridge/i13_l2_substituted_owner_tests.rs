//! Interpreter round i1, wave 13, lane L2
//! (`docs/feature-designs/real-jdk-mode.md`):
//! a direct-call owner the compiler substituted for the constant-pool
//! class is bound by the id the substitution found, without the caller's
//! class-access check; the constant-pool class itself keeps that check.
use super::{
    direct_bind_callee_class, resolve_inline_site, resolve_ir_inline_site, CpResolvers,
    DirectBindClass, SubstitutedOwners,
};
use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState};
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::method::ClassFileMethod;

const CALLER: &str = "cratonvm/i13l2/q/Caller";
/// Package-private, like `java.lang.AbstractStringBuilder`.
const BASE: &str = "cratonvm/i13l2/p/Base";
/// Public and `final`, like `java.lang.StringBuilder`.
const FINAL: &str = "cratonvm/i13l2/p/Final";
/// Package-private AND `final`: an inaccessible constant-pool class.
const SEALED: &str = "cratonvm/i13l2/p/Sealed";
/// Same package as `BASE` / `SEALED`.
const PEER: &str = "cratonvm/i13l2/p/Peer";
const LENGTH: &str = "i13l2Length";
const DESC: &str = "()I";

/// `name()I` without a body (j27 lane INVSP's fixture reuses it).
pub(super) fn method(name: &str, access_flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        access_flags,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(DESC),
        attributes: Vec::new(),
    }
}

/// Define and register a field-less class of the application loader.
pub(super) fn define(
    shared: &SharedVm,
    name: &str,
    access_flags: ClassAccessFlags,
    superclass: Option<ClassId>,
    methods: Vec<ClassFileMethod>,
    pool: Vec<ConstantPoolEntry>,
) -> ClassId {
    let loader = ClassLoaderId::Application;
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
        access_flags,
        superclass: None,
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
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
    });
    cm.register_class_name(loader, name, id);
    if superclass.is_some() {
        cm.set_superclass(id, superclass);
    }
    id
}

/// A pool whose entry 6 is `Methodref <class>.i13l2Length()I`.
fn pool_naming(class: &str) -> Vec<ConstantPoolEntry> {
    vec![
        ConstantPoolEntry::Tombstone,
        ConstantPoolEntry::Utf8(class.into()),
        ConstantPoolEntry::ClassReference { name_index: 1 },
        ConstantPoolEntry::Utf8(LENGTH.into()),
        ConstantPoolEntry::Utf8(DESC.into()),
        ConstantPoolEntry::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        ConstantPoolEntry::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        },
    ]
}

struct Fixture {
    base: ClassId,
    final_class: ClassId,
    /// Names `Final.i13l2Length()I`, from another package.
    caller: ClassId,
    /// Names `Base.i13l2Length()I` directly, from another package.
    direct_caller: ClassId,
}

fn fixture(shared: &SharedVm) -> Fixture {
    let public = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let base = define(
        shared,
        BASE,
        ClassAccessFlags::SUPER,
        None,
        vec![
            method(LENGTH, MethodAccessFlags::PUBLIC),
            method("i13l2Prot", MethodAccessFlags::PROTECTED),
            method("i13l2Pkg", MethodAccessFlags::empty()),
        ],
        vec![ConstantPoolEntry::Tombstone],
    );
    let final_class = define(
        shared,
        FINAL,
        public | ClassAccessFlags::FINAL,
        Some(base),
        vec![],
        vec![ConstantPoolEntry::Tombstone],
    );
    let caller = define(shared, CALLER, public, None, vec![], pool_naming(FINAL));
    let direct_caller = define(
        shared,
        "cratonvm/i13l2/q/DirectCaller",
        public,
        None,
        vec![],
        pool_naming(BASE),
    );
    Fixture {
        base,
        final_class,
        caller,
        direct_caller,
    }
}

fn resolvers(shared: &SharedVm, class_id: ClassId) -> CpResolvers<'_> {
    CpResolvers {
        shared,
        class_id,
        substituted_owners: SubstitutedOwners::default(),
        method_name: "run",
        method_descriptor: "()V",
        code: &[],
        ldc2w_trace: None,
        invoke_operands: Default::default(),
        first_calls_seen: Default::default(),
        deferred_fields_seen: Default::default(),
    }
}

/// Interpreter round i1 wave 17, L3: the declaring-class name and id
/// resolvers a compile request carries for the recursion-cycle checks
/// answer the SAME class (one hierarchy walk): `Final.i13l2Length()I`
/// resolves to `Base`, by name and by id. A site whose constant-pool
/// class the caller may not access answers neither.
#[test]
fn the_declaring_class_name_and_id_resolvers_name_one_class() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let fx = fixture(&shared);
    let cp = resolvers(&shared, fx.caller);
    assert_eq!(cp.invoke_declaring_class(6), Some(BASE.to_string()));
    assert_eq!(cp.invoke_declaring_class_id(6), Some(fx.base.as_u32()));
    // Not a `Methodref`: no answer from either.
    assert_eq!(cp.invoke_declaring_class(2), None);
    assert_eq!(cp.invoke_declaring_class_id(2), None);
    // `Base` is package-private to another package: JVMS 5.4.4 refuses
    // the constant-pool class, so neither resolver names a class.
    let direct = resolvers(&shared, fx.direct_caller);
    assert_eq!(direct.invoke_declaring_class(6), None);
    assert_eq!(direct.invoke_declaring_class_id(6), None);
}

/// The `StringBuilder.length()` shape: the final-owner rule substitutes
/// the package-private declaring class, records it with its id, and the
/// binder answers that id for the caller of another package, marking it
/// inaccessible so only a public (or private) instance member binds.
#[test]
fn a_substituted_package_private_declaring_class_binds_by_its_id() {
    if !crate::runtime::env_cache::jit_final_devirt() {
        return; // `CRATONVM_JIT_FINAL_DEVIRT=0` in this environment
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let fx = fixture(&shared);
    let cp = resolvers(&shared, fx.caller);
    // The answer carries the owner's id to the site's `JitInvokeInfo`
    // (`owner_class_id`), the same id the binder records.
    assert_eq!(
        cp.invokespecial_owner(6, 0xb6),
        Some((BASE.to_string(), fx.base.as_u32()))
    );
    assert_eq!(
        cp.substituted_owners
            .owner_id(fx.caller, BASE, LENGTH, DESC),
        Some(fx.base)
    );
    // Recorded for this caller only.
    assert_eq!(
        cp.substituted_owners
            .owner_id(fx.direct_caller, BASE, LENGTH, DESC),
        None
    );
    let cm = shared.classes.class_manager.read();
    let bound = direct_bind_callee_class(
        &shared,
        &cm,
        &cp.substituted_owners,
        fx.caller,
        BASE,
        LENGTH,
        DESC,
    );
    assert_eq!(
        bound,
        Some(DirectBindClass {
            id: fx.base,
            inaccessible_owner: true,
        })
    );
    // Without the substitution the same name is refused, as before.
    assert_eq!(
        direct_bind_callee_class(
            &shared,
            &cm,
            &SubstitutedOwners::default(),
            fx.caller,
            BASE,
            LENGTH,
            DESC,
        ),
        None
    );
    // The public constant-pool class resolves by name, accessible.
    assert_eq!(
        direct_bind_callee_class(
            &shared,
            &cm,
            &SubstitutedOwners::default(),
            fx.caller,
            FINAL,
            LENGTH,
            DESC,
        ),
        Some(DirectBindClass {
            id: fx.final_class,
            inaccessible_owner: false,
        })
    );
}

/// A genuinely inaccessible REFERENCED class is still refused: a site
/// whose constant pool names the package-private class itself gets no
/// substitution (the private / final rules resolve the constant-pool
/// class through `jit_known_class`) and no bind.
#[test]
fn an_inaccessible_constant_pool_class_is_still_refused() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let fx = fixture(&shared);
    let cp = resolvers(&shared, fx.direct_caller);
    assert_eq!(cp.invokespecial_owner(6, 0xb6), None);
    assert_eq!(
        cp.substituted_owners
            .owner_id(fx.direct_caller, BASE, LENGTH, DESC),
        None
    );
    let cm = shared.classes.class_manager.read();
    assert_eq!(
        direct_bind_callee_class(
            &shared,
            &cm,
            &cp.substituted_owners,
            fx.direct_caller,
            BASE,
            LENGTH,
            DESC,
        ),
        None
    );
}

/// The inverse gap: `invokevirtual_site_final_owner` resolves its
/// constant-pool class through `jit_known_class`, so a package-private
/// `final` class is not devirtualised for a caller of another package,
/// and is for one of its own package.
#[test]
fn the_final_owner_rule_access_checks_the_constant_pool_class() {
    if !crate::runtime::env_cache::jit_final_devirt() {
        return; // `CRATONVM_JIT_FINAL_DEVIRT=0` in this environment
    }
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let fx = fixture(&shared);
    let sealed = define(
        &shared,
        SEALED,
        ClassAccessFlags::SUPER | ClassAccessFlags::FINAL,
        None,
        vec![method(LENGTH, MethodAccessFlags::PUBLIC)],
        vec![ConstantPoolEntry::Tombstone],
    );
    let peer = define(
        &shared,
        PEER,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        None,
        vec![],
        vec![ConstantPoolEntry::Tombstone],
    );
    let cm = shared.classes.class_manager.read();
    let owner = |caller: ClassId| {
        crate::runtime::interpreter::invoke::invokevirtual_site_final_owner(
            &shared, &cm, caller, SEALED, LENGTH, DESC,
        )
    };
    assert_eq!(owner(fx.caller), None);
    assert_eq!(owner(peer), Some((SEALED.to_string(), sealed)));
}

/// Only a public or private INSTANCE member binds through an owner the
/// caller could not name; an accessible owner binds whatever it did.
#[test]
fn an_inaccessible_owner_admits_only_public_or_private_instance_members() {
    let owner = |inaccessible_owner| DirectBindClass {
        id: ClassId::new(7),
        inaccessible_owner,
    };
    let cases = [
        (MethodAccessFlags::PUBLIC, true),
        (MethodAccessFlags::PUBLIC | MethodAccessFlags::FINAL, true),
        (MethodAccessFlags::PRIVATE, true),
        (MethodAccessFlags::PROTECTED, false),
        (MethodAccessFlags::empty(), false),
        (MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC, false),
    ];
    for (flags, admitted) in cases {
        let m = method("m", flags);
        assert_eq!(owner(true).admits(&m), admitted, "{flags:?}");
        assert!(owner(false).admits(&m), "{flags:?}");
    }
}

/// `name()I` with the body `iconst_1; ireturn`, which every inline
/// admission rule accepts.
fn method_returning_one(name: &str, access_flags: MethodAccessFlags) -> ClassFileMethod {
    use cratonvm_reader::attribute::{Attribute, CodeAttribute, LazyAttribute};
    ClassFileMethod {
        access_flags,
        name: std::sync::Arc::from(name),
        descriptor: std::sync::Arc::from(DESC),
        attributes: vec![LazyAttribute::Decoded(Attribute::Code(Box::new(
            CodeAttribute {
                max_stack: 1,
                max_locals: 1,
                code: cratonvm_reader::ByteView::from_vec(vec![0x04, 0xac]),
                exception_table: vec![],
                attributes: vec![],
            },
        )))],
    }
}

/// The package-private `BASE` with bodies, its public `final` subclass,
/// a public NON-final one, and a caller of another package.
fn inline_fixture(shared: &SharedVm) -> (ClassId, ClassId, ClassId) {
    let public = ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER;
    let base = define(
        shared,
        BASE,
        ClassAccessFlags::SUPER,
        None,
        vec![
            method_returning_one(LENGTH, MethodAccessFlags::PUBLIC),
            method_returning_one("i13l2Prot", MethodAccessFlags::PROTECTED),
        ],
        vec![ConstantPoolEntry::Tombstone],
    );
    let final_class = define(
        shared,
        FINAL,
        public | ClassAccessFlags::FINAL,
        Some(base),
        vec![],
        vec![ConstantPoolEntry::Tombstone],
    );
    define(
        shared,
        "cratonvm/i13l2/p/Open",
        public,
        Some(base),
        vec![],
        vec![ConstantPoolEntry::Tombstone],
    );
    let caller = define(shared, CALLER, public, None, vec![], pool_naming(FINAL));
    (base, final_class, caller)
}

/// Interpreter round i1, wave 14, lane L2
/// (`interpreter-L2-inline-resolvers-look-up-a-substituted-owner-by-name`):
/// the single-pass inline resolver splices a substituted package-private
/// declaring class by the id the substitution recorded, where a lookup of
/// its name as the caller's class constant is refused, and applies the
/// direct binder's member rule to it.
#[test]
fn the_inline_resolver_splices_a_substituted_owner_by_its_id() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let (base, _, caller) = inline_fixture(&shared);
    let owners = SubstitutedOwners::default();
    owners.note(caller, BASE, LENGTH, DESC, base);
    owners.note(caller, BASE, "i13l2Prot", DESC, base);
    // Unrecorded: `BASE` is a class constant the caller may not name.
    assert!(resolve_inline_site(&shared, caller, BASE, LENGTH, DESC, None, None).is_none());
    // Recorded: spliced, and the splice names the owner by its id.
    let site = resolve_inline_site(&shared, caller, BASE, LENGTH, DESC, None, Some(&owners))
        .expect("a substituted owner's public member is spliced");
    assert_eq!(site.class_id, base.as_u32());
    assert_eq!(site.class_name, BASE);
    // A protected member of an owner the caller cannot name keeps the call.
    assert!(resolve_inline_site(
        &shared,
        caller,
        BASE,
        "i13l2Prot",
        DESC,
        None,
        Some(&owners)
    )
    .is_none());
    // Recorded for this caller only.
    let other = SubstitutedOwners::default();
    other.note(base, BASE, LENGTH, DESC, base);
    assert!(
        resolve_inline_site(&shared, caller, BASE, LENGTH, DESC, None, Some(&other)).is_none()
    );
}

// The optimizing tier's resolver asks with the constant-pool name: a
// `final` constant-pool class proves the site monomorphic even when the
// body is inherited from a class that is not `final`; a non-`final` one
// does not.
